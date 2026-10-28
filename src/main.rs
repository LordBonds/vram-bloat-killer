//! VRAM Bloat Killer: a tray icon that watches the GPU memory of every process,
//! warns about bloat (browsers over a limit, apps borrowing system RAM, steady
//! leaks), quietly restarts bloated browser GPU processes when you won't notice,
//! can close apps past a hard limit (asking first), and has an emergency wipe.

#![windows_subsystem = "windows"]

mod emergency;
mod gpu;
mod notify;
mod quiet;
mod settings;
mod watch;

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use gpu::{GpuProcess, Sampler, GB};
use notify::{Action, Level};
use settings::{Settings, COOLDOWN_MIN, GROWTH_GB, KILL_GB, WARN_GB};
use watch::{Limits, Reason, Watch};

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const SAMPLE_TIMER: usize = 1;
const SAMPLE_MS: u32 = 5_000;
const COUNTDOWN_TIMER: usize = 2;
const EMERGENCY_HOTKEY: i32 = 1;
/// The same app isn't warned about more than once in this long.
const RENOTIFY: Duration = Duration::from_secs(30 * 60);
/// Past the auto-kill limit, a browser restart waits at most this long for a quiet moment.
const QUIET_MAX_WAIT: Duration = Duration::from_secs(15 * 60);
/// Graceful close: warning time before the app is asked, then time it gets to close.
const GRACE: Duration = Duration::from_secs(30);
const CLOSE_WAIT: Duration = Duration::from_secs(30);
const MENU_ROWS: usize = 10;
/// Keyboard activation of the icon; the windows crate doesn't define it.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

const ID_ROW: u32 = 1000;
const ID_WARN: u32 = 2000;
const ID_GROWTH: u32 = 2100;
const ID_KILL: u32 = 2200;
const ID_KILL_OFF: u32 = 2299;
const ID_COOLDOWN: u32 = 2300;
const ID_KILL_APPS: u32 = 3001;
const ID_STARTUP: u32 = 3002;
const ID_QUIT: u32 = 3003;
const ID_QUIET: u32 = 3004;
const ID_GAME: u32 = 3005;
const ID_GRACEFUL: u32 = 3006;
const ID_FORCE: u32 = 3007;
const ID_POPUPS: u32 = 3008;
const ID_ACTIVITY: u32 = 3009;
const ID_EMERGENCY: u32 = 3010;

const ICON_SIZES: [i32; 5] = [16, 20, 24, 32, 48];
macro_rules! icon_set {
    ($c:literal, $theme:literal) => {
        [
            include_bytes!(concat!("../icons/", $c, "-16-", $theme, ".png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-20-", $theme, ".png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-24-", $theme, ".png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-32-", $theme, ".png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-48-", $theme, ".png")).as_slice(),
        ]
    };
}
/// [dark taskbar, light taskbar][green, amber, red][size]: the dark set wears a paper
/// sticker border, the light set a heavier ink outline.
static ICON_PNGS: [[[&[u8]; 5]; 3]; 2] = [
    [icon_set!("green", "dark"), icon_set!("amber", "dark"), icon_set!("red", "dark")],
    [icon_set!("green", "light"), icon_set!("amber", "light"), icon_set!("red", "light")],
];

/// An app being closed gracefully: warned, then asked, then (optionally) forced.
struct Countdown {
    pid: u32,
    app: String,
    gb: f64,
    asked: bool,
    until: Instant,
}

struct App {
    hwnd: HWND,
    sampler: Sampler,
    watch: Watch,
    settings: Settings,
    procs: Vec<GpuProcess>,
    alerts: HashMap<u32, Reason>,
    icons: [HICON; 3],
    /// The taskbar is in light mode, so the tray uses the ink-outlined icons.
    light_taskbar: bool,
    level: usize,
    /// When each process was last warned about, by pid.
    notified: HashMap<u32, Instant>,
    /// When each app was last killed, by name so a relaunched process still waits.
    last_kill: HashMap<String, Instant>,
    /// Browser GPU processes waiting for a quiet moment, since when.
    waiting: HashMap<u32, Instant>,
    countdowns: Vec<Countdown>,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state, or skips it if the state is busy (a menu's modal loop
/// can deliver timer ticks while a handler is still running).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _single = CreateMutexW(None, true, w!("Local\\VramBloatKiller"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        let sampler = match Sampler::new() {
            Ok(s) => s,
            Err(e) => {
                let text = HSTRING::from(format!("Windows' GPU memory counters are unavailable: {e}"));
                MessageBoxW(None, &text, w!("VRAM Bloat Killer"), MB_ICONERROR);
                return;
            }
        };

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("VramBloatKillerWindow");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // A hidden top-level window rather than a message-only one: only top-level
        // windows hear "TaskbarCreated" when Explorer restarts.
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("VRAM Bloat Killer"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        ) else {
            return;
        };

        // Keep the Run entry pointing at this exe if it was moved.
        if settings::starts_with_windows() {
            settings::set_starts_with_windows(true);
        }

        let light_taskbar = settings::light_taskbar();
        let icons = load_icons(light_taskbar);
        notify::init(hinst.into(), hwnd, app_icon(hinst.into()));
        emergency::init(hinst.into(), hwnd);
        let _ = RegisterHotKey(hwnd, EMERGENCY_HOTKEY, MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, 'W' as u32);

        let mut app = App {
            hwnd,
            sampler,
            watch: Watch::default(),
            settings: Settings::load(),
            procs: Vec::new(),
            alerts: HashMap::new(),
            icons,
            light_taskbar,
            level: 0,
            notified: HashMap::new(),
            last_kill: HashMap::new(),
            waiting: HashMap::new(),
            countdowns: Vec::new(),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        app.procs = app.sampler.sample();
        app.level = level(&app);
        add_tray_icon(&app);
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        SetTimer(hwnd, SAMPLE_TIMER, SAMPLE_MS, None);
        SetTimer(hwnd, COUNTDOWN_TIMER, 1_000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            match wparam.0 {
                SAMPLE_TIMER => with_app(tick),
                COUNTDOWN_TIMER => with_app(countdowns),
                _ => None,
            };
            LRESULT(0)
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam,
            // the click position in wparam.
            match (lparam.0 as u32) & 0xFFFF {
                WM_CONTEXTMENU => {
                    let x = (wparam.0 & 0xFFFF) as i16 as i32;
                    let y = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    show_menu(hwnd, x, y);
                }
                NIN_SELECT | NIN_KEYSELECT => notify::open_activity(),
                _ => {}
            }
            LRESULT(0)
        }
        notify::WM_TOAST_ACTION => {
            let pid = lparam.0 as u32;
            match wparam.0 {
                notify::ACT_RESTART => {
                    with_app(|app| {
                        if let Some(p) = app.procs.iter().find(|p| p.pid == pid).cloned() {
                            restart(app, &p, How::ByHand, true);
                        }
                    });
                }
                notify::ACT_CANCEL => {
                    with_app(|app| cancel_close(app, pid));
                }
                notify::ACT_ACTIVITY => notify::open_activity(),
                notify::ACT_EMERGENCY => open_emergency(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            open_emergency();
            LRESULT(0)
        }
        emergency::WM_EMERGENCY => {
            if let Some(summary) = with_app(|app| emergency_wipe(app, wparam.0 != 0)) {
                emergency::finished(&summary);
            }
            LRESULT(0)
        }
        // The taskbar may have switched between light and dark: swap the tray icon to match.
        WM_SETTINGCHANGE => {
            with_app(|app| {
                let light = settings::light_taskbar();
                if light != app.light_taskbar {
                    let old = std::mem::replace(&mut app.icons, load_icons(light));
                    app.light_taskbar = light;
                    refresh_tray_icon(app);
                    for icon in old {
                        let _ = DestroyIcon(icon);
                    }
                }
            });
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            let nid = base_nid(hwnd);
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let readd = with_app(|app| {
                let hit = app.taskbar_created != 0 && msg == app.taskbar_created;
                if hit {
                    add_tray_icon(app);
                }
                hit
            });
            if readd == Some(true) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

fn limits(s: &Settings) -> Limits {
    let bytes = |gb: f64| (gb * GB) as u64;
    Limits { warn: bytes(s.warn_gb), growth: bytes(s.growth_gb), kill: s.kill_gb.map(bytes) }
}

fn tick(app: &mut App) {
    app.procs = app.sampler.sample();
    notify::set_hidden_ram(app.procs.iter().map(|p| p.shared).sum());
    let alerts = app.watch.assess(&app.procs, &limits(&app.settings));
    app.alerts = alerts.iter().copied().collect();
    let now = Instant::now();
    let procs = app.procs.clone();
    app.notified.retain(|pid, at| procs.iter().any(|p| p.pid == *pid) && at.elapsed() < RENOTIFY);
    let alerting = &app.alerts;
    app.waiting.retain(|pid, _| alerting.contains_key(pid));

    let fullscreen = quiet::fullscreen();
    let hold = app.settings.game_mode && fullscreen;
    // Cards never cover a fullscreen game or video; it all still goes to Activity.
    let popups = app.settings.popups && !fullscreen;
    let cooldown = Duration::from_secs(app.settings.cooldown_min * 60);

    // An action beats any warning; otherwise the first unwarned alert, kill-limit
    // ones first. One thing per tick; the next waits five seconds.
    let mut alerts = alerts;
    alerts.sort_by_key(|(_, r)| *r != Reason::OverKill);
    let mut pending = None;
    for (pid, reason) in alerts {
        let Some(p) = procs.iter().find(|p| p.pid == pid) else { continue };
        let cooled = app.last_kill.get(&p.app).map_or(true, |t| t.elapsed() >= cooldown);
        let closing = app.countdowns.iter().any(|c| c.pid == pid);
        let act = if p.restartable {
            reason == Reason::OverKill || (reason == Reason::OverWarn && app.settings.quiet_wipe)
        } else {
            reason == Reason::OverKill && app.settings.kill_apps && !p.protected && !closing
        };
        if act && cooled {
            if hold {
                if !app.notified.contains_key(&pid) {
                    app.notified.insert(pid, now);
                    notify::post(
                        Level::Info,
                        &format!("Holding off on {}", p.app),
                        &format!("It's holding {:.1} GB, but something is fullscreen. It'll be dealt with once that closes.", p.total_gb()),
                        Action::None,
                        false,
                        None,
                    );
                }
                continue;
            }
            if p.restartable {
                let since = *app.waiting.entry(pid).or_insert(now);
                let unnoticed = quiet::unnoticed(p);
                if unnoticed || (reason == Reason::OverKill && now - since >= QUIET_MAX_WAIT) {
                    app.last_kill.insert(p.app.clone(), now);
                    let how = if unnoticed { How::Quietly } else { How::AfterWaiting };
                    restart(app, p, how, popups);
                    return;
                }
                // Waiting for a moment you won't notice, silently.
                continue;
            }
            app.last_kill.insert(p.app.clone(), now);
            start_close(app, p, popups);
            return;
        }
        if pending.is_none() && !closing && !app.notified.contains_key(&pid) {
            pending = Some((p.clone(), reason, act));
        }
    }
    if let Some((p, reason, cooling)) = pending {
        app.notified.insert(p.pid, now);
        warn(app, &p, reason, cooling, popups);
    }
    refresh_tray_icon(app);
}

fn warn(app: &App, p: &GpuProcess, reason: Reason, cooling: bool, popup: bool) {
    let gb = p.total_gb();
    let mins = app.settings.cooldown_min;
    let (level, title, text) = match reason {
        Reason::OverKill if cooling => (
            Level::Alert,
            format!("{} hit your auto-kill limit again", p.app),
            format!("It's holding {gb:.1} GB. Auto-kill is waiting out its {mins}-minute cooldown."),
        ),
        Reason::OverKill => (
            Level::Alert,
            format!("{} is holding {gb:.1} GB of GPU memory", p.app),
            if !p.protected {
                "That's over your auto-kill limit, but auto-kill only restarts browsers right now. Save your work and restart it when you can.".into()
            } else {
                "That's over your auto-kill limit, but it's part of Windows, so it's never closed. Restarting the PC frees it.".into()
            },
        ),
        Reason::OverWarn if cooling => (
            Level::Warn,
            format!("{}'s GPU memory is back up to {gb:.1} GB", p.app),
            format!("It was restarted less than {mins} minutes ago, so it's waiting out the cooldown."),
        ),
        Reason::OverWarn => (
            Level::Warn,
            format!("{}'s GPU memory hit {gb:.1} GB", p.app),
            "Restarting its GPU process frees it, and your tabs and windows stay open.".into(),
        ),
        Reason::Borrowed => (
            Level::Warn,
            format!("{} borrowed {:.1} GB of system RAM", p.app, p.shared as f64 / GB),
            format!("It's using it as graphics memory, which Task Manager's Memory column doesn't show. {}", fix(p)),
        ),
        Reason::Growing(rise) => (
            Level::Warn,
            format!("{}'s GPU memory keeps climbing", p.app),
            format!("Up {:.1} GB in 30 minutes without dropping back, which looks like a leak. {}", rise as f64 / GB, fix(p)),
        ),
    };
    let action = if p.restartable { Action::Restart(p.pid) } else { Action::None };
    notify::post(level, &title, &text, action, popup, None);
}

/// What the user can do about a bloated app that can't be restarted in place.
fn fix(p: &GpuProcess) -> String {
    if p.protected {
        "It's part of Windows or your graphics driver, so it's never closed. Signing out or restarting the PC frees it.".into()
    } else {
        format!("Save your work and restart {} when you can.", p.app)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum How {
    /// At a moment you weren't looking: logged, no card.
    Quietly,
    /// Over the auto-kill limit and no quiet moment came.
    AfterWaiting,
    ByHand,
}

fn restart(app: &mut App, p: &GpuProcess, how: How, popup: bool) {
    let gb = p.total_gb();
    if gpu::restart(p.pid) {
        app.procs.retain(|q| q.pid != p.pid);
        app.alerts.remove(&p.pid);
        notify::dismiss(p.pid);
        let (title, text) = match how {
            How::Quietly => (
                format!("Freed {gb:.1} GB from {}", p.app),
                "Restarted its GPU process while you weren't looking. Your tabs and windows stayed open.".to_string(),
            ),
            How::AfterWaiting => (
                format!("Restarted {}'s GPU process", p.app),
                format!("It was holding {gb:.1} GB, over your auto-kill limit. Your tabs and windows stay open."),
            ),
            How::ByHand => (format!("Freed {gb:.1} GB from {}", p.app), "Restarted its GPU process. Your tabs and windows stay open.".to_string()),
        };
        notify::post(Level::Info, &title, &text, Action::None, popup && how != How::Quietly, None);
    } else {
        notify::post(
            Level::Alert,
            &format!("Couldn't restart {}'s GPU process", p.app),
            "Windows refused. It may be running as administrator; run VRAM Bloat Killer the same way.",
            Action::None,
            popup,
            None,
        );
    }
    refresh_tray_icon(app);
}

/// Over the auto-kill limit: either a cancellable countdown, then a polite request
/// to close, or (graceful off) closed on the spot.
fn start_close(app: &mut App, p: &GpuProcess, popup: bool) {
    let gb = p.total_gb();
    if app.settings.graceful {
        app.countdowns.push(Countdown { pid: p.pid, app: p.app.clone(), gb, asked: false, until: Instant::now() + GRACE });
        notify::post(
            Level::Alert,
            &format!("Closing {} in {} seconds", p.app, GRACE.as_secs()),
            &format!("It's holding {gb:.1} GB, over your auto-kill limit. Save your work now, or cancel."),
            Action::Cancel(p.pid),
            popup,
            Some(GRACE),
        );
    } else if gpu::close_app(p.pid, &p.app) {
        notify::post(Level::Alert, &format!("Closed {}", p.app), &format!("It was holding {gb:.1} GB, over your auto-kill limit."), Action::None, popup, None);
    }
    refresh_tray_icon(app);
}

fn cancel_close(app: &mut App, pid: u32) {
    let Some(i) = app.countdowns.iter().position(|c| c.pid == pid) else { return };
    let c = app.countdowns.remove(i);
    notify::dismiss(pid);
    notify::post(
        Level::Info,
        &format!("Kept {} open", c.app),
        &format!("Auto-kill will leave it alone for {} minutes.", app.settings.cooldown_min),
        Action::None,
        app.settings.popups,
        None,
    );
}

/// Runs every second while any app is being closed gracefully.
fn countdowns(app: &mut App) {
    if app.countdowns.is_empty() {
        return;
    }
    let now = Instant::now();
    let popup = app.settings.popups && !quiet::fullscreen();
    let force = app.settings.force_after;
    let mut done = Vec::new();
    for (i, c) in app.countdowns.iter_mut().enumerate() {
        let report = |level, title: String, text: String| notify::post(level, &title, &text, Action::None, popup, None);
        if !gpu::is_running(c.pid, &c.app) {
            if c.asked {
                report(Level::Info, format!("{} closed", c.app), format!("It closed when asked, freeing {:.1} GB.", c.gb));
            }
            done.push(i);
        } else if now < c.until {
            continue;
        } else if !c.asked {
            notify::dismiss(c.pid);
            if gpu::ask_to_close(c.pid) > 0 {
                c.asked = true;
                c.until = now + CLOSE_WAIT;
                report(Level::Info, format!("Asked {} to close", c.app), "If it asks about unsaved work, it's waiting for you.".into());
                continue;
            }
            if force && gpu::close_app(c.pid, &c.app) {
                report(Level::Alert, format!("Closed {}", c.app), format!("It has no window to ask, so it was closed. That freed {:.1} GB.", c.gb));
            } else {
                report(Level::Warn, format!("{} has no window to close", c.app), format!("Close it yourself to free {:.1} GB.", c.gb));
            }
            done.push(i);
        } else {
            if force && gpu::close_app(c.pid, &c.app) {
                report(Level::Alert, format!("Force-closed {}", c.app), format!("It didn't close within {} seconds of being asked. That freed {:.1} GB.", CLOSE_WAIT.as_secs(), c.gb));
            } else {
                report(Level::Warn, format!("{} didn't close", c.app), format!("It may be asking you to save. Close it yourself to free {:.1} GB.", c.gb));
            }
            done.push(i);
        }
    }
    for i in done.into_iter().rev() {
        app.countdowns.remove(i);
    }
}

/// The panel, showing exactly what a wipe would touch right now.
fn open_emergency() {
    if let Some(preview) = with_app(|app| {
        app.procs = app.sampler.sample();
        let mut preview = emergency::Preview::default();
        for p in &app.procs {
            let names = if p.restartable {
                &mut preview.restart
            } else if closable(p) && !gpu::windows_of(p.pid).is_empty() {
                &mut preview.close
            } else {
                continue;
            };
            if !names.contains(&p.app) {
                names.push(p.app.clone());
            }
        }
        preview
    }) {
        emergency::open(preview);
    }
}

/// Another app the emergency wipe may ask to close: not part of Windows or a driver, not us.
fn closable(p: &GpuProcess) -> bool {
    !p.restartable && !p.protected && p.pid != std::process::id()
}

/// The emergency wipe: every browser/Electron GPU process restarted, and every other
/// app on the GPU asked to close if `close_others`. Limits and cooldowns don't apply;
/// Windows' own processes are never touched. The summary names everything it touched.
fn emergency_wipe(app: &mut App, close_others: bool) -> String {
    app.procs = app.sampler.sample();
    let now = Instant::now();
    let (mut restarted, mut restarted_gb, mut asked, mut asked_gb) = (Vec::new(), 0.0, Vec::new(), 0.0);
    for p in app.procs.clone() {
        if p.restartable {
            if gpu::restart(p.pid) {
                if !restarted.contains(&p.app) {
                    restarted.push(p.app.clone());
                }
                restarted_gb += p.total_gb();
                app.last_kill.insert(p.app.clone(), now);
            }
        } else if close_others && closable(&p) && gpu::ask_to_close(p.pid) > 0 {
            if !asked.contains(&p.app) {
                asked.push(p.app.clone());
            }
            asked_gb += p.total_gb();
            app.last_kill.insert(p.app.clone(), now);
        }
    }
    let mut summary = if restarted.is_empty() {
        "No browser or Electron app was using the GPU.".to_string()
    } else {
        format!("Restarted the GPU process of {} ({restarted_gb:.1} GB).", emergency::list(&restarted, usize::MAX))
    };
    if close_others {
        summary.push_str(&if asked.is_empty() {
            " No other app needed asking.".to_string()
        } else {
            format!(" Asked {} to close ({asked_gb:.1} GB).", emergency::list(&asked, usize::MAX))
        });
    }
    notify::post(Level::Alert, "Emergency wipe", &summary, Action::None, false, None);
    app.procs.retain(|p| !p.restartable);
    app.alerts.clear();
    refresh_tray_icon(app);
    summary
}

struct MenuState {
    rows: Vec<(u32, String, f64, bool, Option<Reason>)>,
    hidden_gb: f64,
    s: Settings,
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    // Build from a snapshot so the app state isn't borrowed during the menu's modal loop.
    let Some(m) = with_app(|app| MenuState {
        rows: app
            .procs
            .iter()
            .take(MENU_ROWS)
            .map(|p| (p.pid, p.app.clone(), p.total_gb(), p.restartable, app.alerts.get(&p.pid).copied()))
            .collect(),
        hidden_gb: app.procs.iter().map(|p| p.shared).sum::<u64>() as f64 / GB,
        s: app.settings.clone(),
    }) else {
        return;
    };
    let startup = settings::starts_with_windows();

    let chosen = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |menu: HMENU, flags: MENU_ITEM_FLAGS, id: u32, text: &str| {
            let _ = AppendMenuW(menu, flags, id as usize, &HSTRING::from(text));
        };
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        let sub = |parent: HMENU, label: String, items: Vec<(u32, String, bool)>| {
            if let Ok(s) = CreatePopupMenu() {
                for (id, text, on) in items {
                    add(s, MF_STRING | check(on), id, &text);
                }
                let _ = AppendMenuW(parent, MF_POPUP, s.0 as usize, &HSTRING::from(label));
            }
        };
        let separator = |m: HMENU| {
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
        };

        add(menu, MF_STRING, ID_EMERGENCY, "Emergency wipe...\tCtrl+Alt+Shift+W");
        add(menu, MF_STRING, ID_ACTIVITY, "Activity...");
        separator(menu);

        add(menu, MF_STRING | MF_GRAYED, 0, &format!("GPU memory by app ({:.1} GB of RAM borrowed)", m.hidden_gb));
        if m.rows.is_empty() {
            add(menu, MF_STRING | MF_GRAYED, 0, "    Nothing holding over 32 MB");
        }
        for (i, (_, name, gb, restartable, alert)) in m.rows.iter().enumerate() {
            let tag = match alert {
                Some(Reason::OverKill) => "  [over kill limit]",
                Some(Reason::OverWarn | Reason::Borrowed) => "  [over limit]",
                Some(Reason::Growing(_)) => "  [climbing]",
                None => "",
            };
            if *restartable {
                add(menu, MF_STRING, ID_ROW + i as u32, &format!("Restart {name}'s GPU process{tag}\t{gb:.1} GB"));
            } else {
                // Closing a whole app by hand is the app's own job; this is information.
                add(menu, MF_STRING | MF_GRAYED, 0, &format!("{name}{tag}\t{gb:.1} GB"));
            }
        }
        separator(menu);

        let s = &m.s;
        sub(
            menu,
            format!("Warn above: {} GB", s.warn_gb),
            WARN_GB.iter().enumerate().map(|(i, g)| (ID_WARN + i as u32, format!("{g} GB"), *g == s.warn_gb)).collect(),
        );
        sub(
            menu,
            format!("Leak alert: +{} GB in 30 min", s.growth_gb),
            GROWTH_GB.iter().enumerate().map(|(i, g)| (ID_GROWTH + i as u32, format!("+{g} GB"), *g == s.growth_gb)).collect(),
        );
        let mut kills = vec![(ID_KILL_OFF, "Off".to_string(), s.kill_gb.is_none())];
        kills.extend(KILL_GB.iter().enumerate().map(|(i, g)| (ID_KILL + i as u32, format!("{g} GB"), Some(*g) == s.kill_gb)));
        sub(
            menu,
            match s.kill_gb {
                Some(g) => format!("Auto-kill above: {g} GB"),
                None => "Auto-kill: off".into(),
            },
            kills,
        );
        sub(
            menu,
            format!("Auto-kill at most every: {} min", s.cooldown_min),
            COOLDOWN_MIN.iter().enumerate().map(|(i, c)| (ID_COOLDOWN + i as u32, format!("{c} minutes"), *c == s.cooldown_min)).collect(),
        );
        if let Ok(opts) = CreatePopupMenu() {
            add(opts, MF_STRING | check(s.quiet_wipe), ID_QUIET, "Quietly restart browsers at the warn limit");
            add(opts, MF_STRING | check(s.game_mode), ID_GAME, "Game mode: hold auto-kills while fullscreen");
            separator(opts);
            add(opts, MF_STRING | check(s.kill_apps), ID_KILL_APPS, "Auto-kill closes other apps too");
            add(opts, MF_STRING | check(s.graceful) | if s.kill_apps { MF_ENABLED } else { MF_GRAYED }, ID_GRACEFUL, "Ask them to close first (30 s warning)");
            add(opts, MF_STRING | check(s.force_after) | if s.kill_apps && s.graceful { MF_ENABLED } else { MF_GRAYED }, ID_FORCE, "Force-close if they don't (unsaved work is lost)");
            separator(opts);
            add(opts, MF_STRING | check(s.popups), ID_POPUPS, "Pop-up notifications");
            add(opts, MF_STRING | check(startup), ID_STARTUP, "Start with Windows");
            let _ = AppendMenuW(menu, MF_POPUP, opts.0 as usize, w!("Options"));
        }
        separator(menu);
        add(menu, MF_STRING, ID_QUIT, "Quit");

        // Without this the menu won't close when you click elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, None).0 as u32;
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        id
    };

    let in_range = |id: u32, base: u32, len: usize| (base..base + len as u32).contains(&id);
    let change = |f: &dyn Fn(&mut Settings)| {
        with_app(|app| {
            f(&mut app.settings);
            app.settings.save();
            app.notified.clear();
            refresh_tray_icon(app);
        });
    };
    match chosen {
        0 => {}
        ID_QUIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        ID_EMERGENCY => open_emergency(),
        ID_ACTIVITY => notify::open_activity(),
        ID_STARTUP => settings::set_starts_with_windows(!startup),
        ID_KILL_APPS => change(&|s| s.kill_apps = !s.kill_apps),
        ID_QUIET => change(&|s| s.quiet_wipe = !s.quiet_wipe),
        ID_GAME => change(&|s| s.game_mode = !s.game_mode),
        ID_GRACEFUL => change(&|s| s.graceful = !s.graceful),
        ID_FORCE => change(&|s| s.force_after = !s.force_after),
        ID_POPUPS => change(&|s| s.popups = !s.popups),
        ID_KILL_OFF => change(&|s| s.kill_gb = None),
        id if in_range(id, ID_WARN, WARN_GB.len()) => change(&|s| s.warn_gb = WARN_GB[(id - ID_WARN) as usize]),
        id if in_range(id, ID_GROWTH, GROWTH_GB.len()) => change(&|s| s.growth_gb = GROWTH_GB[(id - ID_GROWTH) as usize]),
        id if in_range(id, ID_KILL, KILL_GB.len()) => change(&|s| s.kill_gb = Some(KILL_GB[(id - ID_KILL) as usize])),
        id if in_range(id, ID_COOLDOWN, COOLDOWN_MIN.len()) => {
            change(&|s| s.cooldown_min = COOLDOWN_MIN[(id - ID_COOLDOWN) as usize])
        }
        id if in_range(id, ID_ROW, m.rows.len()) => {
            let pid = m.rows[(id - ID_ROW) as usize].0;
            with_app(|app| {
                if let Some(p) = app.procs.iter().find(|p| p.pid == pid).cloned() {
                    restart(app, &p, How::ByHand, true);
                }
            });
        }
        _ => {}
    }
}

/// 0 green, 1 amber (something over half the warn limit), 2 red (an alert is active).
fn level(app: &App) -> usize {
    if !app.alerts.is_empty() {
        return 2;
    }
    let half = app.settings.warn_gb * GB / 2.0;
    let near = app.procs.iter().any(|p| (if p.restartable { p.total() } else { p.shared }) as f64 >= half);
    usize::from(near)
}

fn tooltip(app: &App) -> String {
    let mut tip = String::from("VRAM Bloat Killer");
    if app.procs.is_empty() {
        tip.push_str("\nNothing holding over 32 MB");
    }
    for p in app.procs.iter().take(3) {
        tip.push_str(&format!("\n{} {:.1} GB", p.app, p.total_gb()));
    }
    let s = &app.settings;
    match s.kill_gb {
        Some(k) => tip.push_str(&format!("\nWarn {} GB, kill {k} GB", s.warn_gb)),
        None => tip.push_str(&format!("\nWarn {} GB, auto-kill off", s.warn_gb)),
    }
    tip
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: TRAY_ID, ..Default::default() }
}

fn add_tray_icon(app: &App) {
    let mut nid = base_nid(app.hwnd);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = app.icons[app.level];
    copy_wide(&mut nid.szTip, &tooltip(app));
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
}

fn refresh_tray_icon(app: &mut App) {
    app.level = level(app);
    let mut nid = base_nid(app.hwnd);
    nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.hIcon = app.icons[app.level];
    copy_wide(&mut nid.szTip, &tooltip(app));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Copies into a fixed UTF-16 field, truncating and always null-terminating.
fn copy_wide(dst: &mut [u16], src: &str) {
    let room = dst.len() - 1;
    let mut n = 0;
    for (slot, unit) in dst.iter_mut().zip(src.encode_utf16()).take(room) {
        *slot = unit;
        n += 1;
    }
    dst[n] = 0;
}

fn load_icons(light_taskbar: bool) -> [HICON; 3] {
    let theme = usize::from(light_taskbar);
    let load = |level: usize| unsafe {
        let want = GetSystemMetrics(SM_CXSMICON);
        let i = ICON_SIZES.iter().position(|&s| s >= want).unwrap_or(ICON_SIZES.len() - 1);
        CreateIconFromResourceEx(ICON_PNGS[theme][level][i], true, 0x0003_0000, want, want, LR_DEFAULTCOLOR).unwrap_or_default()
    };
    [load(0), load(1), load(2)]
}

/// The app icon for windows: the one stamped into the exe (tools/stamp_exe.py), or the
/// green tray balloon in a build that hasn't been stamped.
fn app_icon(hinst: HINSTANCE) -> HICON {
    unsafe {
        LoadIconW(hinst, PCWSTR(1 as *const u16)).unwrap_or_else(|_| {
            CreateIconFromResourceEx(ICON_PNGS[0][0][3], true, 0x0003_0000, 32, 32, LR_DEFAULTCOLOR).unwrap_or_default()
        })
    }
}
