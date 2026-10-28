//! VRAM Bloat Killer: a tray icon that watches the GPU memory of every process,
//! warns about bloat (browsers over a limit, apps borrowing system RAM, steady
//! leaks), and can kill past a hard limit. A browser/Electron GPU process is
//! restarted, which keeps every tab open; other apps are closed only if allowed.

#![windows_subsystem = "windows"]

mod gpu;
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
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use gpu::{GpuProcess, Sampler, GB};
use settings::{Settings, COOLDOWN_MIN, GROWTH_GB, KILL_GB, WARN_GB};
use watch::{Limits, Reason, Watch};

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const TIMER_ID: usize = 1;
const SAMPLE_MS: u32 = 5_000;
/// The same app isn't notified about more than once in this long.
const RENOTIFY: Duration = Duration::from_secs(30 * 60);
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

const ICON_SIZES: [i32; 5] = [16, 20, 24, 32, 48];
macro_rules! icon_set {
    ($c:literal) => {
        [
            include_bytes!(concat!("../icons/", $c, "-16.png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-20.png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-24.png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-32.png")).as_slice(),
            include_bytes!(concat!("../icons/", $c, "-48.png")).as_slice(),
        ]
    };
}
static ICON_PNGS: [[&[u8]; 5]; 3] = [icon_set!("green"), icon_set!("amber"), icon_set!("red")];

struct App {
    hwnd: HWND,
    sampler: Sampler,
    watch: Watch,
    settings: Settings,
    procs: Vec<GpuProcess>,
    alerts: HashMap<u32, Reason>,
    icons: [HICON; 3],
    level: usize,
    /// When each app was last notified about, by pid.
    notified: HashMap<u32, Instant>,
    /// When each app was last auto-killed, by name so a relaunched process still waits.
    last_kill: HashMap<String, Instant>,
    /// The process a click on the current notification restarts.
    balloon_pid: Option<u32>,
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

        let mut app = App {
            hwnd,
            sampler,
            watch: Watch::default(),
            settings: Settings::load(),
            procs: Vec::new(),
            alerts: HashMap::new(),
            icons: [load_icon(0), load_icon(1), load_icon(2)],
            level: 0,
            notified: HashMap::new(),
            last_kill: HashMap::new(),
            balloon_pid: None,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        app.procs = app.sampler.sample();
        app.level = level(&app);
        add_tray_icon(&app);
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        SetTimer(hwnd, TIMER_ID, SAMPLE_MS, None);

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
            with_app(tick);
            LRESULT(0)
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam,
            // the click position in wparam.
            match (lparam.0 as u32) & 0xFFFF {
                WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => {
                    let x = (wparam.0 & 0xFFFF) as i16 as i32;
                    let y = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    show_menu(hwnd, x, y);
                }
                NIN_BALLOONUSERCLICK => {
                    with_app(|app| {
                        if let Some(pid) = app.balloon_pid.take() {
                            kill(app, pid, false);
                        }
                    });
                }
                NIN_BALLOONTIMEOUT => {
                    with_app(|app| app.balloon_pid = None);
                }
                _ => {}
            }
            LRESULT(0)
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
    let alerts = app.watch.assess(&app.procs, &limits(&app.settings));
    app.alerts = alerts.iter().copied().collect();
    let now = Instant::now();
    let procs = app.procs.clone();
    app.notified.retain(|pid, at| procs.iter().any(|p| p.pid == *pid) && at.elapsed() < RENOTIFY);
    let cooldown = Duration::from_secs(app.settings.cooldown_min * 60);

    // A kill beats any notification; otherwise the first unnotified alert, kill-limit
    // ones first. One action per tick; the next waits five seconds.
    let mut alerts = alerts;
    alerts.sort_by_key(|(_, r)| *r != Reason::OverKill);
    let mut pending = None;
    for (pid, reason) in alerts {
        let Some(p) = procs.iter().find(|p| p.pid == pid) else { continue };
        let may_kill = reason == Reason::OverKill && (p.restartable || (app.settings.kill_apps && gpu::closable(&p.app)));
        let cooled = app.last_kill.get(&p.app).map_or(true, |t| t.elapsed() >= cooldown);
        if may_kill && cooled {
            app.last_kill.insert(p.app.clone(), now);
            kill(app, pid, true);
            return;
        }
        if pending.is_none() && !app.notified.contains_key(&pid) {
            pending = Some((p.clone(), reason, may_kill));
        }
    }
    if let Some((p, reason, cooling)) = pending {
        app.notified.insert(p.pid, now);
        notify(app, &p, reason, cooling);
    }
    refresh_tray_icon(app);
}

fn notify(app: &mut App, p: &GpuProcess, reason: Reason, cooling: bool) {
    let gb = p.total_gb();
    let mins = app.settings.cooldown_min;
    let (title, text) = match reason {
        Reason::OverKill if cooling => (
            format!("{} hit your auto-kill limit again: {gb:.1} GB", p.app),
            if p.restartable {
                format!("Auto-kill is waiting out its {mins}-minute cooldown. Click here to restart its GPU process now.")
            } else {
                format!("Auto-kill is waiting out its {mins}-minute cooldown. Save your work.")
            },
        ),
        Reason::OverKill => (
            format!("{} is holding {gb:.1} GB of GPU memory", p.app),
            if gpu::closable(&p.app) {
                "That's over your auto-kill limit, but closing apps other than browsers is turned off. Save your work and restart it when you can.".into()
            } else {
                "That's over your auto-kill limit, but it's part of Windows, so it's never closed. Restarting the PC frees it.".into()
            },
        ),
        Reason::OverWarn => (
            format!("{}'s GPU memory hit {gb:.1} GB", p.app),
            "Click here to restart its GPU process. Your tabs and windows stay open.".into(),
        ),
        Reason::Borrowed => (
            format!("{} borrowed {:.1} GB of system RAM", p.app, p.shared as f64 / GB),
            format!("It's using it as graphics memory, which Task Manager's Memory column doesn't show. Restart {} when you can.", p.app),
        ),
        Reason::Growing(rise) => (
            format!("{}'s GPU memory keeps climbing", p.app),
            format!(
                "Up {:.1} GB in 30 minutes without dropping back, which looks like a leak. Save your work and restart {} when you can.",
                rise as f64 / GB,
                p.app
            ),
        ),
    };
    app.balloon_pid = p.restartable.then_some(p.pid);
    let icon = if reason == Reason::OverKill { NIIF_ERROR } else { NIIF_WARNING };
    balloon(app, &title, &text, icon);
}

/// Restarts a browser/Electron GPU process, or closes any other app.
fn kill(app: &mut App, pid: u32, auto: bool) {
    let Some(p) = app.procs.iter().find(|p| p.pid == pid).cloned() else { return };
    let gb = p.total_gb();
    let why = if auto { ", over your auto-kill limit" } else { "" };
    let (ok, title, text) = if p.restartable {
        (
            gpu::restart(pid),
            format!("Restarted {}'s GPU process", p.app),
            format!("It was holding {gb:.1} GB{why}. Your tabs and windows stay open."),
        )
    } else {
        (gpu::close_app(pid, &p.app), format!("Closed {}", p.app), format!("It was holding {gb:.1} GB of GPU memory{why}."))
    };
    if ok {
        app.procs.retain(|q| q.pid != pid);
        app.alerts.remove(&pid);
        balloon(app, &title, &text, NIIF_INFO);
    } else {
        balloon(
            app,
            &format!("Couldn't stop {}", p.app),
            "Windows refused. It may be running as administrator; run VRAM Bloat Killer the same way.",
            NIIF_ERROR,
        );
    }
    refresh_tray_icon(app);
}

struct MenuState {
    rows: Vec<(u32, String, f64, bool, Option<Reason>)>,
    warn: f64,
    growth: f64,
    kill: Option<f64>,
    kill_apps: bool,
    cooldown: u64,
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
        warn: app.settings.warn_gb,
        growth: app.settings.growth_gb,
        kill: app.settings.kill_gb,
        kill_apps: app.settings.kill_apps,
        cooldown: app.settings.cooldown_min,
    }) else {
        return;
    };
    let startup = settings::starts_with_windows();

    let chosen = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |menu: HMENU, flags: MENU_ITEM_FLAGS, id: u32, text: &str| {
            let _ = AppendMenuW(menu, flags, id as usize, &HSTRING::from(text));
        };
        let sub = |label: String, items: Vec<(u32, String, bool)>| {
            if let Ok(s) = CreatePopupMenu() {
                for (id, text, on) in items {
                    add(s, MF_STRING | if on { MF_CHECKED } else { MF_UNCHECKED }, id, &text);
                }
                let _ = AppendMenuW(menu, MF_POPUP, s.0 as usize, &HSTRING::from(label));
            }
        };
        let separator = || {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        };

        add(menu, MF_STRING | MF_GRAYED, 0, "GPU memory by app");
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
        separator();

        sub(
            format!("Warn above: {} GB", m.warn),
            WARN_GB.iter().enumerate().map(|(i, g)| (ID_WARN + i as u32, format!("{g} GB"), *g == m.warn)).collect(),
        );
        sub(
            format!("Leak alert: +{} GB in 30 min", m.growth),
            GROWTH_GB.iter().enumerate().map(|(i, g)| (ID_GROWTH + i as u32, format!("+{g} GB"), *g == m.growth)).collect(),
        );
        let mut kills = vec![(ID_KILL_OFF, "Off".to_string(), m.kill.is_none())];
        kills.extend(KILL_GB.iter().enumerate().map(|(i, g)| (ID_KILL + i as u32, format!("{g} GB"), Some(*g) == m.kill)));
        sub(
            match m.kill {
                Some(g) => format!("Auto-kill above: {g} GB"),
                None => "Auto-kill: off".into(),
            },
            kills,
        );
        sub(
            format!("Auto-kill at most every: {} min", m.cooldown),
            COOLDOWN_MIN.iter().enumerate().map(|(i, c)| (ID_COOLDOWN + i as u32, format!("{c} minutes"), *c == m.cooldown)).collect(),
        );
        add(
            menu,
            MF_STRING | if m.kill_apps { MF_CHECKED } else { MF_UNCHECKED },
            ID_KILL_APPS,
            "Auto-kill closes other apps too (unsaved work is lost)",
        );
        separator();
        add(menu, MF_STRING | if startup { MF_CHECKED } else { MF_UNCHECKED }, ID_STARTUP, "Start with Windows");
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
        ID_STARTUP => settings::set_starts_with_windows(!startup),
        ID_KILL_APPS => change(&|s| s.kill_apps = !s.kill_apps),
        ID_KILL_OFF => change(&|s| s.kill_gb = None),
        id if in_range(id, ID_WARN, WARN_GB.len()) => change(&|s| s.warn_gb = WARN_GB[(id - ID_WARN) as usize]),
        id if in_range(id, ID_GROWTH, GROWTH_GB.len()) => change(&|s| s.growth_gb = GROWTH_GB[(id - ID_GROWTH) as usize]),
        id if in_range(id, ID_KILL, KILL_GB.len()) => change(&|s| s.kill_gb = Some(KILL_GB[(id - ID_KILL) as usize])),
        id if in_range(id, ID_COOLDOWN, COOLDOWN_MIN.len()) => {
            change(&|s| s.cooldown_min = COOLDOWN_MIN[(id - ID_COOLDOWN) as usize])
        }
        id if in_range(id, ID_ROW, m.rows.len()) => {
            let pid = m.rows[(id - ID_ROW) as usize].0;
            with_app(|app| kill(app, pid, false));
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

fn balloon(app: &App, title: &str, text: &str, kind: NOTIFY_ICON_INFOTIP_FLAGS) {
    let mut nid = base_nid(app.hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = kind;
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide(&mut nid.szInfo, text);
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

fn load_icon(level: usize) -> HICON {
    unsafe {
        let want = GetSystemMetrics(SM_CXSMICON);
        let i = ICON_SIZES.iter().position(|&s| s >= want).unwrap_or(ICON_SIZES.len() - 1);
        CreateIconFromResourceEx(ICON_PNGS[level][i], true, 0x0003_0000, want, want, LR_DEFAULTCOLOR).unwrap_or_default()
    }
}
