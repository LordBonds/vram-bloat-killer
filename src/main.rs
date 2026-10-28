//! VRAM Bloat Killer: a tray icon that watches the GPU memory of browser and app
//! GPU processes, warns when one passes your limit, and restarts it on request
//! (or automatically). Restarting a Chromium GPU process keeps every tab open.

#![windows_subsystem = "windows"]

mod gpu;
mod settings;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use gpu::{GpuProcess, Sampler, GB};
use settings::{Settings, LIMITS_GB};

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const TIMER_ID: usize = 1;
const SAMPLE_MS: u32 = 5_000;
/// Chromium gives up on hardware acceleration if its GPU process dies too often,
/// so automatic restarts of the same app are spaced out.
const AUTO_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const MENU_ROWS: usize = 10;

/// Keyboard activation of the icon; the windows crate doesn't define it.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

const ID_RESTART: u32 = 1000;
const ID_LIMIT: u32 = 2000;
const ID_AUTO: u32 = 3001;
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
    settings: Settings,
    procs: Vec<GpuProcess>,
    icons: [HICON; 3],
    level: usize,
    /// GPU processes already warned about; cleared once they drop back under 80% of the limit.
    warned: HashSet<u32>,
    last_auto: HashMap<String, Instant>,
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
            settings: Settings::load(),
            procs: Vec::new(),
            icons: [load_icon(0), load_icon(1), load_icon(2)],
            level: 0,
            warned: HashSet::new(),
            last_auto: HashMap::new(),
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
                            restart(app, pid);
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

fn tick(app: &mut App) {
    app.procs = app.sampler.sample();
    let limit = (app.settings.limit_gb * GB) as u64;
    let rearm = limit / 10 * 8;
    let procs = app.procs.clone();
    app.warned.retain(|pid| procs.iter().any(|p| p.pid == *pid && p.total() >= rearm));

    for p in procs.iter().filter(|p| p.total() >= limit) {
        if app.warned.contains(&p.pid) {
            continue;
        }
        app.warned.insert(p.pid);
        let cooled = app.last_auto.get(&p.app).map_or(true, |t| t.elapsed() >= AUTO_COOLDOWN);
        if app.settings.auto_restart && cooled {
            app.last_auto.insert(p.app.clone(), Instant::now());
            restart(app, p.pid);
        } else {
            app.balloon_pid = Some(p.pid);
            balloon(
                app,
                &format!("{}'s GPU memory hit {:.1} GB", p.app, p.total_gb()),
                "Click here to restart its GPU process. Your tabs and windows stay open.",
                NIIF_WARNING,
            );
        }
        // One notification per tick; the next one waits five seconds.
        break;
    }
    refresh_tray_icon(app);
}

fn restart(app: &mut App, pid: u32) {
    let Some(p) = app.procs.iter().find(|p| p.pid == pid).cloned() else { return };
    if gpu::restart(pid) {
        app.procs.retain(|q| q.pid != pid);
        balloon(
            app,
            &format!("Restarted {}'s GPU process", p.app),
            &format!("It was holding {:.1} GB. Your tabs and windows stay open.", p.total_gb()),
            NIIF_INFO,
        );
    } else {
        balloon(
            app,
            &format!("Couldn't restart {}'s GPU process", p.app),
            "Windows refused. It may be running as administrator; run VRAM Bloat Killer the same way.",
            NIIF_ERROR,
        );
    }
    refresh_tray_icon(app);
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    // Build from a snapshot so the app state isn't borrowed during the menu's modal loop.
    let Some((rows, limit_gb, auto)) =
        with_app(|app| (app.procs.iter().take(MENU_ROWS).map(|p| (p.pid, p.app.clone(), p.total_gb())).collect::<Vec<_>>(), app.settings.limit_gb, app.settings.auto_restart))
    else {
        return;
    };
    let startup = settings::starts_with_windows();

    let chosen = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |m: HMENU, flags: MENU_ITEM_FLAGS, id: u32, text: &str| {
            let _ = AppendMenuW(m, flags, id as usize, &HSTRING::from(text));
        };
        let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };

        add(menu, MF_STRING | MF_GRAYED, 0, "GPU memory by app (click to restart)");
        if rows.is_empty() {
            add(menu, MF_STRING | MF_GRAYED, 0, "    Nothing holding over 32 MB");
        }
        for (i, (_, name, gb)) in rows.iter().enumerate() {
            add(menu, MF_STRING, ID_RESTART + i as u32, &format!("Restart {name}'s GPU process\t{gb:.1} GB"));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        if let Ok(limits) = CreatePopupMenu() {
            for (i, gb) in LIMITS_GB.iter().enumerate() {
                add(limits, MF_STRING | checked(*gb == limit_gb), ID_LIMIT + i as u32, &format!("{gb} GB"));
            }
            let _ = AppendMenuW(menu, MF_POPUP, limits.0 as usize, &HSTRING::from(format!("Limit per app: {limit_gb} GB")));
        }
        add(menu, MF_STRING | checked(auto), ID_AUTO, "Restart automatically over the limit");
        add(menu, MF_STRING | checked(startup), ID_STARTUP, "Start with Windows");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        add(menu, MF_STRING, ID_QUIT, "Quit");

        // Without this the menu won't close when you click elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, None).0 as u32;
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        id
    };

    match chosen {
        0 => {}
        ID_QUIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        ID_AUTO => {
            with_app(|app| {
                app.settings.auto_restart = !app.settings.auto_restart;
                app.settings.save();
            });
        }
        ID_STARTUP => settings::set_starts_with_windows(!startup),
        id if (ID_LIMIT..ID_LIMIT + LIMITS_GB.len() as u32).contains(&id) => {
            with_app(|app| {
                app.settings.limit_gb = LIMITS_GB[(id - ID_LIMIT) as usize];
                app.settings.save();
                app.warned.clear();
                refresh_tray_icon(app);
            });
        }
        id if (ID_RESTART..ID_RESTART + rows.len() as u32).contains(&id) => {
            let pid = rows[(id - ID_RESTART) as usize].0;
            with_app(|app| restart(app, pid));
        }
        _ => {}
    }
}

/// 0 green (under half the limit), 1 amber, 2 red (something is over).
fn level(app: &App) -> usize {
    let top = app.procs.first().map_or(0.0, |p| p.total_gb());
    if top >= app.settings.limit_gb {
        2
    } else if top >= app.settings.limit_gb / 2.0 {
        1
    } else {
        0
    }
}

fn tooltip(app: &App) -> String {
    let mut tip = String::from("VRAM Bloat Killer");
    if app.procs.is_empty() {
        tip.push_str("\nNo app GPU memory over 32 MB");
    }
    for p in app.procs.iter().take(3) {
        tip.push_str(&format!("\n{} {:.1} GB", p.app, p.total_gb()));
    }
    tip.push_str(&format!("\nLimit {} GB{}", app.settings.limit_gb, if app.settings.auto_restart { ", auto" } else { "" }));
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
