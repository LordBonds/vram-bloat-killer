//! VRAM Bloat Killer's own notifications, in place of Windows' toasts: small dark
//! cards at the bottom-right that never take focus or make a sound, fade out on
//! their own (and wait while the mouse is on them), and are all kept in an
//! Activity window you can scroll back through, saved across restarts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{w, HSTRING};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the main window when a card's button or body is clicked:
/// wparam is one of the ACT_ codes, lparam the pid it concerns.
pub const WM_TOAST_ACTION: u32 = WM_APP + 2;
pub const ACT_RESTART: usize = 1;
pub const ACT_CANCEL: usize = 2;
pub const ACT_ACTIVITY: usize = 3;
pub const ACT_EMERGENCY: usize = 4;

/// Not exported by the windows crate in this version.
const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Clone, Copy, PartialEq)]
pub enum Level {
    Info,
    Warn,
    Alert,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    None,
    /// "Restart now": restart this GPU process.
    Restart(u32),
    /// "Cancel": stop the countdown to closing this app.
    Cancel(u32),
}

pub struct Entry {
    pub at: u64,
    pub level: Level,
    pub title: String,
    pub text: String,
}

const MAX_CARDS: usize = 3;
const MAX_ENTRIES: usize = 500;
const FRAME_MS: u32 = 30;
const FADE: Duration = Duration::from_millis(180);
/// After the mouse leaves a card it stays this long before fading.
const LINGER: Duration = Duration::from_millis(1500);

// Colours (COLORREF is 0x00BBGGRR).
pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}
pub(crate) const CARD_BG: COLORREF = rgb(32, 34, 40);
const PAGE_BG: COLORREF = rgb(24, 26, 30);
pub(crate) const HOVER_BG: COLORREF = rgb(50, 54, 62);
pub(crate) const RULE: COLORREF = rgb(44, 47, 55);
pub(crate) const TEXT: COLORREF = rgb(236, 237, 240);
pub(crate) const SUBTLE: COLORREF = rgb(150, 156, 166);

fn level_colour(level: Level) -> COLORREF {
    match level {
        Level::Info => rgb(63, 185, 80),
        Level::Warn => rgb(230, 162, 40),
        Level::Alert => rgb(248, 81, 73),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Body,
    Close,
    Button,
}

struct Card {
    level: Level,
    title: String,
    text: String,
    action: Action,
    /// Countdown cards: (start, end) for the progress bar.
    countdown: Option<(Instant, Instant)>,
    born: Instant,
    hide_at: Instant,
    hover: Option<Part>,
    closing: Option<Instant>,
    title_h: i32,
    body_h: i32,
}

struct Fonts {
    small: HFONT,
    title: HFONT,
    body: HFONT,
    heading: HFONT,
}

struct Ui {
    main: HWND,
    hinst: HINSTANCE,
    dpi: i32,
    fonts: Fonts,
    cards: Vec<HWND>,
    card_state: HashMap<isize, Card>,
    entries: Vec<Entry>,
    activity: Option<HWND>,
    scroll: i32,
    /// Which header button the mouse is on: 1 Clear, 2 Emergency wipe.
    header_hover: u8,
    hidden_ram: u64,
}

impl Ui {
    fn px(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

pub fn init(hinst: HINSTANCE, main: HWND, icon: HICON) {
    unsafe {
        let card = WNDCLASSW {
            lpfnWndProc: Some(card_proc),
            hInstance: hinst,
            lpszClassName: w!("VramBloatKillerCard"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&card);
        let page = WNDCLASSW {
            lpfnWndProc: Some(activity_proc),
            hInstance: hinst,
            lpszClassName: w!("VramBloatKillerActivity"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: icon,
            ..Default::default()
        };
        RegisterClassW(&page);
    }
    let dpi = unsafe { GetDpiForSystem() } as i32;
    let font = |px: i32, weight: i32| font(px, weight, dpi);
    let fonts = Fonts { small: font(12, 400), title: font(14, 600), body: font(13, 400), heading: font(20, 600) };
    let ui = Ui {
        main,
        hinst,
        dpi,
        fonts,
        cards: Vec::new(),
        card_state: HashMap::new(),
        entries: load_log(),
        activity: None,
        scroll: 0,
        header_hover: 0,
        hidden_ram: 0,
    };
    UI.with(|cell| *cell.borrow_mut() = Some(ui));
}

/// Records an event in the Activity log and, if `popup`, shows it as a card.
/// `countdown` makes the card stay until that long has passed, with a progress bar.
pub fn post(level: Level, title: &str, text: &str, action: Action, popup: bool, countdown: Option<Duration>) {
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let entry = Entry { at, level, title: title.to_string(), text: text.to_string() };
    append_log(&entry);
    let activity = with(|ui| {
        ui.entries.push(entry);
        if ui.entries.len() > MAX_ENTRIES {
            let extra = ui.entries.len() - MAX_ENTRIES;
            ui.entries.drain(..extra);
            save_log(&ui.entries);
        }
        ui.activity
    })
    .flatten();
    if let Some(hwnd) = activity {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
    if popup {
        show_card(level, title, text, action, countdown);
    }
}

/// Fades out any card offering an action on this pid (its countdown ended or was cancelled).
pub fn dismiss(pid: u32) {
    with(|ui| {
        let now = Instant::now();
        for card in ui.card_state.values_mut() {
            if matches!(card.action, Action::Restart(p) | Action::Cancel(p) if p == pid) && card.closing.is_none() {
                card.closing = Some(now);
            }
        }
    });
}

/// The Activity window's headline: system RAM the GPUs are borrowing right now.
pub fn set_hidden_ram(bytes: u64) {
    if let Some(Some(hwnd)) = with(|ui| {
        let changed = ui.hidden_ram / (64 << 20) != bytes / (64 << 20);
        ui.hidden_ram = bytes;
        ui.activity.filter(|_| changed)
    }) {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

// ---------------------------------------------------------------- cards

fn show_card(level: Level, title: &str, text: &str, action: Action, countdown: Option<Duration>) {
    let Some((hinst, width, title_h, body_h, height)) = with(|ui| {
        let width = ui.px(360);
        let text_w = width - ui.px(4 + 16 + 16);
        let title_h = measure(ui.fonts.title, title, text_w).min(ui.px(20) * 2);
        let body_h = measure(ui.fonts.body, text, text_w).min(ui.px(18) * 4);
        (ui.hinst, width, title_h, body_h, card_height(ui, title_h, body_h, action))
    }) else {
        return;
    };
    let now = Instant::now();
    let life = match (countdown, action, level) {
        (Some(d), _, _) => d + Duration::from_millis(600),
        (None, Action::None, Level::Info) => Duration::from_secs(5),
        _ => Duration::from_secs(8),
    };
    unsafe {
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            w!("VramBloatKillerCard"),
            &HSTRING::from(title),
            WS_POPUP,
            0,
            0,
            width,
            height,
            None,
            None,
            hinst,
            None,
        ) else {
            return;
        };
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        let card = Card {
            level,
            title: title.to_string(),
            text: text.to_string(),
            action,
            countdown: countdown.map(|d| (now, now + d)),
            born: now,
            hide_at: now + life,
            hover: None,
            closing: None,
            title_h,
            body_h,
        };
        let overflow = with(|ui| {
            ui.card_state.insert(key(hwnd), card);
            ui.cards.push(hwnd);
            if ui.cards.len() > MAX_CARDS {
                Some(ui.cards[0])
            } else {
                None
            }
        })
        .flatten();
        if let Some(old) = overflow {
            let _ = DestroyWindow(old);
        }
        layout_cards();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, 1, FRAME_MS, None);
    }
}

fn card_height(ui: &Ui, title_h: i32, body_h: i32, action: Action) -> i32 {
    let button = if action == Action::None { 0 } else { ui.px(10 + 28) };
    ui.px(12 + 16 + 6) + title_h + ui.px(2) + body_h + button + ui.px(14)
}

/// Newest at the bottom, stacked upwards from the corner above the taskbar.
fn layout_cards() {
    let Some((cards, gap)) = with(|ui| (ui.cards.clone(), ui.px(10))) else { return };
    unsafe {
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let mut bottom = work.bottom - gap;
        for hwnd in cards.iter().rev() {
            let mut r = RECT::default();
            let _ = GetWindowRect(*hwnd, &mut r);
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            bottom -= h;
            let _ = SetWindowPos(*hwnd, HWND_TOPMOST, work.right - gap - w, bottom, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            bottom -= gap;
        }
    }
}

/// Where a card's parts are, for both painting and clicking.
fn card_parts(ui: &Ui, card: &Card, client: RECT) -> (RECT, Option<RECT>) {
    let s = |v| ui.px(v);
    let close = RECT { left: client.right - s(34), top: s(6), right: client.right - s(8), bottom: s(32) };
    let button = (card.action != Action::None).then(|| {
        let label = button_label(card.action);
        let w = measure_width(ui.fonts.title, label) + s(24);
        RECT { left: client.right - s(14) - w, top: client.bottom - s(14) - s(28), right: client.right - s(14), bottom: client.bottom - s(14) }
    });
    (close, button)
}

fn button_label(action: Action) -> &'static str {
    match action {
        Action::Restart(_) => "Restart now",
        Action::Cancel(_) => "Cancel",
        Action::None => "",
    }
}

fn part_at(ui: &Ui, card: &Card, client: RECT, pt: POINT) -> Part {
    let (close, button) = card_parts(ui, card, client);
    let inside = |r: RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
    if inside(close) {
        Part::Close
    } else if button.is_some_and(inside) {
        Part::Button
    } else {
        Part::Body
    }
}

unsafe extern "system" fn card_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_card(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER => {
            card_frame(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let changed = with(|ui| {
                let part = ui.card_state.get(&key(hwnd)).map(|c| part_at(ui, c, client, pt));
                let card = ui.card_state.get_mut(&key(hwnd))?;
                let was = card.hover;
                card.hover = part;
                Some(was != part)
            })
            .flatten();
            let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            let _ = TrackMouseEvent(&mut track);
            if changed == Some(true) {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with(|ui| {
                if let Some(card) = ui.card_state.get_mut(&key(hwnd)) {
                    card.hover = None;
                    card.hide_at = card.hide_at.max(Instant::now() + LINGER);
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let pointer = with(|ui| ui.card_state.get(&key(hwnd)).is_some_and(|c| matches!(c.hover, Some(Part::Close | Part::Button))));
            let cursor = if pointer == Some(true) { IDC_HAND } else { IDC_ARROW };
            SetCursor(LoadCursorW(None, cursor).unwrap_or_default());
            LRESULT(1)
        }
        WM_LBUTTONUP => {
            // Act on where the click landed, not on the last hover.
            let pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let clicked = with(|ui| {
                let part = ui.card_state.get(&key(hwnd)).map(|c| part_at(ui, c, client, pt));
                let card = ui.card_state.get_mut(&key(hwnd))?;
                card.closing.get_or_insert(Instant::now());
                Some((part, card.action, ui.main))
            })
            .flatten();
            if let Some((part, action, main)) = clicked {
                let post = |code: usize, pid: u32| {
                    let _ = PostMessageW(main, WM_TOAST_ACTION, WPARAM(code), LPARAM(pid as isize));
                };
                match (part, action) {
                    (Some(Part::Button), Action::Restart(pid)) => post(ACT_RESTART, pid),
                    (Some(Part::Button), Action::Cancel(pid)) => post(ACT_CANCEL, pid),
                    (Some(Part::Body), _) => post(ACT_ACTIVITY, 0),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            with(|ui| {
                ui.card_state.remove(&key(hwnd));
                ui.cards.retain(|h| *h != hwnd);
            });
            layout_cards();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn card_frame(hwnd: HWND) {
    let now = Instant::now();
    let Some((alpha, done, ticking)) = with(|ui| {
        let card = ui.card_state.get_mut(&key(hwnd))?;
        if card.closing.is_none() && card.hover.is_none() && now >= card.hide_at {
            card.closing = Some(now);
        }
        let fade = |t: Duration| (t.as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
        let alpha = match card.closing {
            Some(at) => 1.0 - fade(now - at),
            None => fade(now - card.born),
        };
        Some(((alpha * 255.0) as u8, card.closing.is_some() && alpha <= 0.0, card.countdown.is_some()))
    })
    .flatten() else {
        return;
    };
    unsafe {
        if done {
            let _ = DestroyWindow(hwnd);
            return;
        }
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        if ticking {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn paint_card(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        with(|ui| {
            let Some(card) = ui.card_state.get(&key(hwnd)) else { return };
            buffered(hdc, client, |dc| {
                let s = |v| ui.px(v);
                fill(dc, client, CARD_BG);
                let accent = level_colour(card.level);
                fill(dc, RECT { right: s(4), ..client }, accent);

                let left = s(4 + 16);
                let (close, button) = card_parts(ui, card, client);
                text(dc, ui.fonts.small, SUBTLE, "VRAM Bloat Killer", RECT { left, top: s(12), right: close.left, bottom: s(28) }, DT_SINGLELINE);
                if card.hover == Some(Part::Close) {
                    fill(dc, close, HOVER_BG);
                }
                text(dc, ui.fonts.body, if card.hover == Some(Part::Close) { TEXT } else { SUBTLE }, "\u{00D7}", close, DT_SINGLELINE | DT_CENTER | DT_VCENTER);

                let top = s(12 + 16 + 6);
                let right = client.right - s(16);
                text(dc, ui.fonts.title, TEXT, &card.title, RECT { left, top, right, bottom: top + card.title_h }, DT_WORDBREAK | DT_EDITCONTROL | DT_END_ELLIPSIS);
                let top = top + card.title_h + s(2);
                text(dc, ui.fonts.body, SUBTLE, &card.text, RECT { left, top, right, bottom: top + card.body_h }, DT_WORDBREAK | DT_EDITCONTROL | DT_END_ELLIPSIS);

                if let Some(b) = button {
                    fill(dc, b, if card.hover == Some(Part::Button) { HOVER_BG } else { rgb(42, 45, 52) });
                    text(dc, ui.fonts.title, accent, button_label(card.action), b, DT_SINGLELINE | DT_CENTER | DT_VCENTER);
                }
                if let Some((start, end)) = card.countdown {
                    let total = (end - start).as_secs_f32().max(0.001);
                    let left_frac = 1.0 - ((Instant::now() - start).as_secs_f32() / total).clamp(0.0, 1.0);
                    let w = ((client.right - s(4)) as f32 * left_frac) as i32;
                    fill(dc, RECT { left: s(4), top: client.bottom - s(3), right: s(4) + w, bottom: client.bottom }, accent);
                }
            });
        });
        let _ = EndPaint(hwnd, &ps);
    }
}

// ---------------------------------------------------------------- activity window

pub fn open_activity() {
    let existing = with(|ui| ui.activity).flatten();
    unsafe {
        if let Some(hwnd) = existing {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            return;
        }
        let Some((hinst, w, h)) = with(|ui| (ui.hinst, ui.px(480), ui.px(620))) else { return };
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("VramBloatKillerActivity"),
            w!("VRAM Bloat Killer \u{2014} Activity"),
            WS_OVERLAPPEDWINDOW,
            work.right - w - 24,
            work.bottom - h - 24,
            w,
            h,
            None,
            None,
            hinst,
            None,
        ) else {
            return;
        };
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        with(|ui| {
            ui.activity = Some(hwnd);
            ui.scroll = 0;
        });
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

const HEADER: i32 = 92;

fn clear_rect(ui: &Ui, client: RECT) -> RECT {
    RECT { left: client.right - ui.px(84), top: ui.px(18), right: client.right - ui.px(16), bottom: ui.px(42) }
}

fn emergency_rect(ui: &Ui, client: RECT) -> RECT {
    let clear = clear_rect(ui, client);
    RECT { left: clear.left - ui.px(132), right: clear.left - ui.px(8), ..clear }
}

unsafe extern "system" fn activity_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_activity(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            with(|ui| ui.scroll = (ui.scroll - delta * ui.px(48) / 120).max(0));
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_MOUSEMOVE | WM_LBUTTONUP => {
            let pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let inside = |r: RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
            let (changed, emergency) = with(|ui| {
                let over = if !ui.entries.is_empty() && inside(clear_rect(ui, client)) {
                    1
                } else if inside(emergency_rect(ui, client)) {
                    2
                } else {
                    0
                };
                if msg == WM_LBUTTONUP && over == 1 {
                    ui.entries.clear();
                    ui.scroll = 0;
                    save_log(&ui.entries);
                    return (true, None);
                }
                if msg == WM_LBUTTONUP && over == 2 {
                    return (false, Some(ui.main));
                }
                let changed = ui.header_hover != over;
                ui.header_hover = over;
                (changed, None)
            })
            .unwrap_or((false, None));
            if let Some(main) = emergency {
                let _ = PostMessageW(main, WM_TOAST_ACTION, WPARAM(ACT_EMERGENCY), LPARAM(0));
            }
            if changed {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let hand = with(|ui| ui.header_hover != 0) == Some(true);
            SetCursor(LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).unwrap_or_default());
            LRESULT(1)
        }
        WM_GETMINMAXINFO => {
            if let Some(px) = with(|ui| (ui.px(360), ui.px(320))) {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT { x: px.0, y: px.1 };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            with(|ui| ui.activity = None);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn paint_activity(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        with(|ui| {
            let dpi = ui.dpi;
            buffered(hdc, client, |dc| {
                let s = |v: i32| v * dpi / 96;
                fill(dc, client, PAGE_BG);
                let pad = s(20);
                let right = client.right - pad;

                // Rows first, so the header paints over anything scrolled under it.
                let list_top = s(HEADER);
                let text_w = right - (pad + s(18));
                let when_room = s(110);
                let rows: Vec<(i32, i32, &Entry)> = ui
                    .entries
                    .iter()
                    .rev()
                    .map(|e| (measure(ui.fonts.title, &e.title, text_w - when_room).max(s(20)), measure(ui.fonts.body, &e.text, text_w), e))
                    .collect();
                let row_height = |title_h: i32, body_h: i32| s(14) + title_h + s(4) + body_h + s(14);
                let total: i32 = rows.iter().map(|(t, b, _)| row_height(*t, *b)).sum();
                let visible = client.bottom - list_top;
                ui.scroll = ui.scroll.min((total - visible).max(0));
                let mut y = list_top - ui.scroll;
                for (title_h, body_h, e) in &rows {
                    let row_h = row_height(*title_h, *body_h);
                    if y + row_h > list_top && y < client.bottom {
                        let top = y + s(14);
                        fill(dc, RECT { left: pad, top: top + s(6), right: pad + s(8), bottom: top + s(14) }, level_colour(e.level));
                        let x = pad + s(18);
                        let when = when(e.at);
                        let when_w = measure_width(ui.fonts.small, &when);
                        text(dc, ui.fonts.small, SUBTLE, &when, RECT { left: right - when_w, top: top + s(2), right, bottom: top + s(20) }, DT_SINGLELINE | DT_RIGHT);
                        text(dc, ui.fonts.title, TEXT, &e.title, RECT { left: x, top, right: right - when_room, bottom: top + title_h }, DT_WORDBREAK | DT_EDITCONTROL);
                        let body_top = top + title_h + s(4);
                        text(dc, ui.fonts.body, SUBTLE, &e.text, RECT { left: x, top: body_top, right, bottom: body_top + body_h }, DT_WORDBREAK | DT_EDITCONTROL);
                        fill(dc, RECT { left: pad, top: y + row_h - 1, right, bottom: y + row_h }, RULE);
                    }
                    y += row_h;
                }
                if rows.is_empty() {
                    let r = RECT { left: pad, top: list_top + s(24), right, bottom: list_top + s(80) };
                    text(dc, ui.fonts.body, SUBTLE, "Nothing yet. Bloat warnings, restarts and closes will show up here.", r, DT_WORDBREAK | DT_CENTER);
                }

                fill(dc, RECT { bottom: list_top, ..client }, PAGE_BG);
                text(dc, ui.fonts.heading, TEXT, "Activity", RECT { left: pad, top: s(14), right, bottom: s(44) }, DT_SINGLELINE);
                let hidden = format!(
                    "GPUs are borrowing {:.1} GB of your system RAM right now. Task Manager's Memory column doesn't show it.",
                    ui.hidden_ram as f64 / (1u64 << 30) as f64
                );
                text(dc, ui.fonts.small, SUBTLE, &hidden, RECT { left: pad, top: s(48), right, bottom: s(HEADER - 8) }, DT_WORDBREAK);
                if !ui.entries.is_empty() {
                    let r = clear_rect(ui, client);
                    if ui.header_hover == 1 {
                        fill(dc, r, HOVER_BG);
                    }
                    text(dc, ui.fonts.body, if ui.header_hover == 1 { TEXT } else { SUBTLE }, "Clear", r, DT_SINGLELINE | DT_CENTER | DT_VCENTER);
                }
                let r = emergency_rect(ui, client);
                fill(dc, r, if ui.header_hover == 2 { rgb(120, 34, 30) } else { rgb(84, 28, 26) });
                text(dc, ui.fonts.body, rgb(255, 214, 206), "Emergency wipe", r, DT_SINGLELINE | DT_CENTER | DT_VCENTER);
                fill(dc, RECT { left: 0, top: list_top - 1, right: client.right, bottom: list_top }, RULE);
            });
        });
        let _ = EndPaint(hwnd, &ps);
    }
}

// ---------------------------------------------------------------- drawing helpers

/// Segoe UI at `px` logical pixels.
pub(crate) fn font(px: i32, weight: i32, dpi: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -(px * dpi / 96),
        lfWeight: weight,
        lfCharSet: DEFAULT_CHARSET,
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    for (slot, unit) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
        *slot = unit;
    }
    unsafe { CreateFontIndirectW(&lf) }
}
/// Paints into an off-screen bitmap and copies it over in one go, so nothing flickers.
pub(crate) unsafe fn buffered(hdc: HDC, client: RECT, draw: impl FnOnce(HDC)) {
    let (w, h) = (client.right.max(1), client.bottom.max(1));
    let dc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT);
    draw(dc);
    let _ = BitBlt(hdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(dc);
}

pub(crate) unsafe fn fill(dc: HDC, r: RECT, colour: COLORREF) {
    let brush = CreateSolidBrush(colour);
    FillRect(dc, &r, brush);
    let _ = DeleteObject(brush);
}

pub(crate) unsafe fn text(dc: HDC, font: HFONT, colour: COLORREF, s: &str, mut r: RECT, flags: DRAW_TEXT_FORMAT) {
    let old = SelectObject(dc, font);
    SetTextColor(dc, colour);
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(dc, &mut wide, &mut r, flags | DT_NOPREFIX);
    SelectObject(dc, old);
}

/// Height of `s` wrapped to `width`.
pub(crate) fn measure(font: HFONT, s: &str, width: i32) -> i32 {
    calc(font, s, RECT { left: 0, top: 0, right: width, bottom: 0 }, DT_WORDBREAK | DT_EDITCONTROL).bottom
}

pub(crate) fn measure_width(font: HFONT, s: &str) -> i32 {
    calc(font, s, RECT::default(), DT_SINGLELINE).right
}

fn calc(font: HFONT, s: &str, mut r: RECT, flags: DRAW_TEXT_FORMAT) -> RECT {
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, font);
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        DrawTextW(dc, &mut wide, &mut r, flags | DT_CALCRECT | DT_NOPREFIX);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
    }
    r
}

/// "1:32 AM" today, "Oct 27, 1:32 AM" before that.
fn when(at: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = |secs: u64| unsafe {
        let ticks = (secs + 11_644_473_600) * 10_000_000;
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        let mut utc = SYSTEMTIME::default();
        let mut local = SYSTEMTIME::default();
        let _ = FileTimeToSystemTime(&ft, &mut utc);
        let _ = SystemTimeToTzSpecificLocalTime(None, &utc, &mut local);
        local
    };
    let t = local(at);
    let now = local(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    let hour = match t.wHour % 12 {
        0 => 12,
        h => h,
    };
    let clock = format!("{hour}:{:02} {}", t.wMinute, if t.wHour < 12 { "AM" } else { "PM" });
    if (t.wYear, t.wMonth, t.wDay) == (now.wYear, now.wMonth, now.wDay) {
        clock
    } else {
        format!("{} {}, {clock}", MONTHS[(t.wMonth.clamp(1, 12) - 1) as usize], t.wDay)
    }
}

// ---------------------------------------------------------------- the saved log

fn log_file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("VRAM Bloat Killer").join("activity.log")
}

fn log_line(e: &Entry) -> String {
    let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
    let level = match e.level {
        Level::Info => 0,
        Level::Warn => 1,
        Level::Alert => 2,
    };
    format!("{}\t{}\t{}\t{}\n", e.at, level, clean(&e.title), clean(&e.text))
}

fn parse_line(line: &str) -> Option<Entry> {
    let mut f = line.splitn(4, '\t');
    let at = f.next()?.parse().ok()?;
    let level = match f.next()? {
        "0" => Level::Info,
        "1" => Level::Warn,
        _ => Level::Alert,
    };
    Some(Entry { at, level, title: f.next()?.to_string(), text: f.next()?.to_string() })
}

fn load_log() -> Vec<Entry> {
    let text = std::fs::read_to_string(log_file()).unwrap_or_default();
    let mut entries: Vec<Entry> = text.lines().filter_map(parse_line).collect();
    let extra = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..extra);
    entries
}

fn append_log(e: &Entry) {
    let path = log_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(log_line(e).as_bytes());
    }
}

fn save_log(entries: &[Entry]) {
    let _ = std::fs::write(log_file(), entries.iter().map(log_line).collect::<String>());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_round_trip_and_stay_one_line() {
        let e = Entry { at: 1_800_000_000, level: Level::Warn, title: "Opera\thit 3 GB".into(), text: "two\nlines".into() };
        let line = log_line(&e);
        assert_eq!(line.matches('\n').count(), 1);
        let back = parse_line(line.trim_end()).unwrap();
        assert_eq!(back.at, e.at);
        assert!(back.level == Level::Warn);
        assert_eq!(back.title, "Opera hit 3 GB");
        assert_eq!(back.text, "two lines");
    }

    #[test]
    fn bad_lines_are_skipped() {
        assert!(parse_line("not a number\t1\tx\ty").is_none());
        assert!(parse_line("123\t1\tonly title").is_none());
    }
}
