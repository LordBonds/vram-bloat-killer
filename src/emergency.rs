//! The emergency wipe panel: a big red button under a glass cover. Lifting the
//! cover is the "are you sure"; pressing the button flashes, shakes, and asks the
//! main window to wipe everything (see `emergency_wipe` in main.rs).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::notify::{buffered, fill, font, measure, text, CARD_BG, HOVER_BG, RULE, SUBTLE, TEXT};

/// Posted to the main window to run the wipe; wparam is 1 to close other apps too.
pub const WM_EMERGENCY: u32 = WM_APP + 3;

const W: i32 = 420;
const H: i32 = 620;
/// The art is drawn in this square, its top this far down the panel.
const ART: i32 = 360;
const ART_TOP: i32 = 64;
/// In the art's own 520-pixel canvas: where the plate, cover and button sit.
const CANVAS: i32 = 520;
const COVER_HALF: i32 = 96;
const BUTTON_R: i32 = 70;
const LIFT: Duration = Duration::from_millis(220);
/// Once the cover is up, the button stays dead this long: a fast double or triple
/// click can never lift the cover and fire in one go.
const ARM: Duration = Duration::from_millis(700);
const BAM: Duration = Duration::from_millis(520);
/// After the wipe the panel closes itself, unless the mouse is on it.
const DONE_LINGER: Duration = Duration::from_secs(6);

static ART_PNGS: [&[u8]; 6] = [
    include_bytes!("../art/plate.png"),
    include_bytes!("../art/button.png"),
    include_bytes!("../art/button_hot.png"),
    include_bytes!("../art/button_down.png"),
    include_bytes!("../art/cover_closed.png"),
    include_bytes!("../art/cover_open.png"),
];
const PLATE: usize = 0;
const BUTTON: usize = 1;
const BUTTON_HOT: usize = 2;
const BUTTON_DOWN: usize = 3;
const COVER_CLOSED: usize = 4;
const COVER_OPEN: usize = 5;

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Closed,
    Lifting(Instant),
    /// The cover is up, since this moment; the button works once ARM has passed.
    Open(Instant),
    Bam(Instant),
    Working,
    Done(Instant),
}

#[derive(Clone, Copy, PartialEq)]
enum Spot {
    None,
    Cover,
    Button,
    Check,
    Cancel,
}

struct Panel {
    main: HWND,
    hinst: HINSTANCE,
    dpi: i32,
    hwnd: Option<HWND>,
    origin: POINT,
    stage: Stage,
    hover: Spot,
    close_others: bool,
    preview: Preview,
    summary: String,
    /// Decoded art at the current DPI: (bitmap, size in pixels).
    art: Vec<(HBITMAP, i32)>,
    fonts: [HFONT; 4],
}

impl Panel {
    fn px(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }
}

thread_local! {
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Panel) -> R) -> Option<R> {
    PANEL.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

pub fn init(hinst: HINSTANCE, main: HWND) {
    let dpi = unsafe { GetDpiForSystem() } as i32;
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            hInstance: hinst,
            lpszClassName: w!("VramBloatKillerEmergency"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
    }
    let panel = Panel {
        main,
        hinst,
        dpi,
        hwnd: None,
        origin: POINT::default(),
        stage: Stage::Closed,
        hover: Spot::None,
        close_others: true,
        preview: Preview::default(),
        summary: String::new(),
        art: Vec::new(),
        fonts: [font(12, 400, dpi), font(13, 400, dpi), font(14, 600, dpi), font(22, 700, dpi)],
    };
    PANEL.with(|cell| *cell.borrow_mut() = Some(panel));
}

/// What a wipe would do right now, shown before you press: whose GPU processes get
/// restarted, and which other apps get asked to close.
#[derive(Default, Clone)]
pub struct Preview {
    pub restart: Vec<String>,
    pub close: Vec<String>,
}

/// "Opera, Steam, Claude and 2 more"
pub fn list(names: &[String], max: usize) -> String {
    let mut shown: Vec<&str> = names.iter().take(max).map(String::as_str).collect();
    if names.len() > max {
        let more = format!("{} more", names.len() - max);
        return format!("{} and {more}", shown.join(", "));
    }
    match shown.len() {
        0 => String::new(),
        1 => shown[0].to_string(),
        _ => {
            let last = shown.pop().unwrap_or_default();
            format!("{} and {last}", shown.join(", "))
        }
    }
}

/// Opens the panel (cover down) showing `preview`, or refreshes it and brings it to the front.
pub fn open(preview: Preview) {
    let Some((existing, hinst, w, h)) = with(|p| {
        p.preview = preview;
        (p.hwnd, p.hinst, p.px(W), p.px(H))
    }) else {
        return;
    };
    unsafe {
        if let Some(hwnd) = existing {
            let _ = InvalidateRect(hwnd, None, false);
            let _ = SetForegroundWindow(hwnd);
            return;
        }
        if with(|p| p.art.is_empty()).unwrap_or(false) {
            let size = with(|p| p.px(ART)).unwrap_or(ART);
            let art: Vec<(HBITMAP, i32)> = ART_PNGS.iter().map(|png| (decode(png, size).unwrap_or_default(), size)).collect();
            with(|p| p.art = art);
        }
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let origin = POINT { x: (work.left + work.right - w) / 2, y: (work.top + work.bottom - h) / 2 };
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("VramBloatKillerEmergency"),
            w!("Emergency wipe"),
            WS_POPUP,
            origin.x,
            origin.y,
            w,
            h,
            None,
            None,
            hinst,
            None,
        ) else {
            return;
        };
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
        with(|p| {
            p.hwnd = Some(hwnd);
            p.origin = origin;
            p.stage = Stage::Closed;
            p.hover = Spot::None;
            p.summary.clear();
        });
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        SetTimer(hwnd, 1, 16, None);
    }
}

/// Called by the main window once the wipe has run.
pub fn finished(summary: &str) {
    let hwnd = with(|p| {
        p.summary = summary.to_string();
        p.stage = Stage::Done(Instant::now());
        p.hwnd
    })
    .flatten();
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

/// PNG bytes -> a premultiplied 32-bit DIB of `size` x `size`, scaled with WIC's best filter.
unsafe fn decode(png: &[u8], size: i32) -> windows::core::Result<HBITMAP> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = factory.CreateStream()?;
    stream.InitializeFromMemory(png)?;
    let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
    let frame = decoder.GetFrame(0)?;
    let scaler = factory.CreateBitmapScaler()?;
    scaler.Initialize(&frame, size as u32, size as u32, WICBitmapInterpolationModeHighQualityCubic)?;
    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom)?;

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let len = (size * size * 4) as usize;
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, len);
    converter.CopyPixels(std::ptr::null(), (size * 4) as u32, buf)?;
    Ok(bmp)
}

unsafe fn blend(dc: HDC, art: (HBITMAP, i32), x: i32, y: i32, alpha: u8) {
    let src = CreateCompatibleDC(dc);
    let old = SelectObject(src, art.0);
    let f = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
    let _ = AlphaBlend(dc, x, y, art.1, art.1, src, 0, 0, art.1, art.1, f);
    SelectObject(src, old);
    let _ = DeleteDC(src);
}

/// Where things are on the panel, in client pixels.
struct Layout {
    art: RECT,
    cover: RECT,
    button_centre: POINT,
    button_r: i32,
    check: RECT,
    cancel: RECT,
}

fn layout(p: &Panel, client: RECT) -> Layout {
    let s = |v| p.px(v);
    let size = s(ART);
    let art = RECT { left: (client.right - size) / 2, top: s(ART_TOP), right: (client.right + size) / 2, bottom: s(ART_TOP) + size };
    let k = |v: i32| v * size / CANVAS;
    let cx = (art.left + art.right) / 2;
    let cy = (art.top + art.bottom) / 2;
    let cover = RECT { left: cx - k(COVER_HALF), top: cy - k(COVER_HALF + 10), right: cx + k(COVER_HALF), bottom: cy + k(COVER_HALF + 22) };
    let check = RECT { left: s(28), top: s(442), right: client.right - s(28), bottom: s(492) };
    let cancel = RECT { left: client.right - s(128), top: client.bottom - s(52), right: client.right - s(28), bottom: client.bottom - s(18) };
    Layout { art, cover, button_centre: POINT { x: cx, y: cy }, button_r: k(BUTTON_R), check, cancel }
}

fn spot_at(p: &Panel, client: RECT, pt: POINT) -> Spot {
    let l = layout(p, client);
    let inside = |r: &RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
    let (dx, dy) = ((pt.x - l.button_centre.x) as i64, (pt.y - l.button_centre.y) as i64);
    let on_button = dx * dx + dy * dy <= (l.button_r as i64).pow(2);
    match p.stage {
        _ if inside(&l.cancel) => Spot::Cancel,
        Stage::Closed if inside(&l.cover) => Spot::Cover,
        Stage::Open(t) if on_button && t.elapsed() >= ARM => Spot::Button,
        Stage::Closed | Stage::Open(_) if inside(&l.check) => Spot::Check,
        _ => Spot::None,
    }
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            frame(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let changed = with(|p| {
                let spot = spot_at(p, client, pt);
                let changed = spot != p.hover;
                p.hover = spot;
                changed
            });
            if changed == Some(true) {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            let hand = with(|p| p.hover != Spot::None) == Some(true);
            SetCursor(LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).unwrap_or_default());
            LRESULT(1)
        }
        WM_LBUTTONUP => {
            let now = Instant::now();
            // Act on where the click landed, not on the last hover.
            let pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let close = with(|p| match spot_at(p, client, pt) {
                Spot::Cancel => true,
                Spot::Cover => {
                    p.stage = Stage::Lifting(now);
                    p.hover = Spot::None;
                    false
                }
                Spot::Button => {
                    p.stage = Stage::Bam(now);
                    p.hover = Spot::None;
                    false
                }
                Spot::Check => {
                    p.close_others = !p.close_others;
                    false
                }
                Spot::None => false,
            });
            if close == Some(true) {
                let _ = DestroyWindow(hwnd);
            } else {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // Drag the panel by anything that isn't a control.
        WM_NCHITTEST => {
            let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
            let mut pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            let _ = ScreenToClient(hwnd, &mut pt);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let free = with(|p| spot_at(p, client, pt) == Spot::None && !matches!(p.stage, Stage::Bam(_))) == Some(true);
            if hit.0 == HTCLIENT as isize && free && pt.y < with(|p| p.px(ART_TOP)).unwrap_or(0) {
                LRESULT(HTCAPTION as isize)
            } else {
                hit
            }
        }
        WM_MOVE => {
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            with(|p| {
                if !matches!(p.stage, Stage::Bam(_)) {
                    p.origin = POINT { x: r.left, y: r.top };
                }
            });
            LRESULT(0)
        }
        WM_DESTROY => {
            with(|p| p.hwnd = None);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Animation: the cover lifting, the BAM (flash + shake), and closing when done.
fn frame(hwnd: HWND) {
    let now = Instant::now();
    let mut destroy = false;
    let mut wipe = None;
    let mut shake = None;
    let mut repaint = false;
    with(|p| match p.stage {
        Stage::Lifting(t) => {
            repaint = true;
            if now - t >= LIFT {
                p.stage = Stage::Open(now);
            }
        }
        // Repaint once more when the button comes alive.
        Stage::Open(t) if now - t < ARM + Duration::from_millis(50) => repaint = true,
        Stage::Bam(t) => {
            repaint = true;
            let e = (now - t).as_secs_f32() / BAM.as_secs_f32();
            let amp = (1.0 - e).max(0.0) * p.px(10) as f32;
            let dx = ((now - t).as_secs_f32() * 70.0).sin() * amp;
            shake = Some(POINT { x: p.origin.x + dx as i32, y: p.origin.y });
            if e >= 1.0 {
                p.stage = Stage::Working;
                shake = Some(p.origin);
                wipe = Some((p.main, p.close_others));
            }
        }
        Stage::Done(t) => {
            let hovered = unsafe {
                let mut pt = POINT::default();
                let mut r = RECT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = GetWindowRect(hwnd, &mut r);
                pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
            };
            if hovered {
                p.stage = Stage::Done(now);
            } else if now - t >= DONE_LINGER {
                destroy = true;
            }
        }
        _ => {}
    });
    unsafe {
        if let Some(pt) = shake {
            let _ = SetWindowPos(hwnd, None, pt.x, pt.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        if repaint {
            let _ = InvalidateRect(hwnd, None, false);
        }
        if let Some((main, close_others)) = wipe {
            let _ = PostMessageW(main, WM_EMERGENCY, WPARAM(close_others as usize), LPARAM(0));
        }
        if destroy {
            let _ = DestroyWindow(hwnd);
        }
    }
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        with(|p| {
            let l = layout(p, client);
            let [small, body, bold, heading] = p.fonts;
            let now = Instant::now();
            buffered(hdc, client, |dc| {
                let s = |v| p.px(v);
                fill(dc, client, CARD_BG);
                fill(dc, RECT { bottom: s(4), ..client }, rgb_red());
                let pad = s(28);
                let right = client.right - pad;
                text(dc, heading, TEXT, "Emergency wipe", RECT { left: pad, top: s(22), right, bottom: s(54) }, DT_SINGLELINE);

                // The art: plate, button, cover.
                if p.art.len() == ART_PNGS.len() {
                    let (x, y) = (l.art.left, l.art.top);
                    blend(dc, p.art[PLATE], x, y, 255);
                    let button = match p.stage {
                        Stage::Bam(_) | Stage::Working | Stage::Done(_) => BUTTON_DOWN,
                        Stage::Open(_) if p.hover == Spot::Button => BUTTON_HOT,
                        _ => BUTTON,
                    };
                    blend(dc, p.art[button], x, y, 255);
                    match p.stage {
                        Stage::Closed => blend(dc, p.art[COVER_CLOSED], x, y, if p.hover == Spot::Cover { 255 } else { 230 }),
                        Stage::Lifting(t) => {
                            let e = ((now - t).as_secs_f32() / LIFT.as_secs_f32()).clamp(0.0, 1.0);
                            blend(dc, p.art[COVER_CLOSED], x, y, ((1.0 - e) * 230.0) as u8);
                            blend(dc, p.art[COVER_OPEN], x, y, (e * 255.0) as u8);
                        }
                        _ => blend(dc, p.art[COVER_OPEN], x, y, 255),
                    }
                }

                // What happens next.
                let (hint, hint_colour) = match p.stage {
                    Stage::Closed => ("Lift the cover to arm it.".to_string(), if p.hover == Spot::Cover { TEXT } else { SUBTLE }),
                    Stage::Open(t) if t.elapsed() >= ARM => ("Armed. Press the button to wipe.".to_string(), TEXT),
                    Stage::Lifting(_) | Stage::Open(_) => ("Arming...".to_string(), SUBTLE),
                    Stage::Bam(_) | Stage::Working => ("Wiping...".to_string(), TEXT),
                    Stage::Done(_) => (p.summary.clone(), TEXT),
                };
                let hint_rect = RECT { left: pad, top: s(392), right, bottom: s(436) };
                text(dc, bold, hint_colour, &hint, hint_rect, DT_CENTER | DT_WORDBREAK);

                // The option, then what it all does.
                let armed_or_closed = matches!(p.stage, Stage::Closed | Stage::Open(_) | Stage::Lifting(_));
                if armed_or_closed {
                    if p.hover == Spot::Check {
                        fill(dc, l.check, HOVER_BG);
                    }
                    let b = RECT { left: l.check.left + s(10), top: l.check.top + s(8), right: l.check.left + s(28), bottom: l.check.top + s(26) };
                    if p.close_others {
                        fill(dc, b, rgb_red());
                        tick_mark(dc, b, s(2));
                    } else {
                        let pen_brush = CreateSolidBrush(SUBTLE);
                        FrameRect(dc, &b, pen_brush);
                        let _ = DeleteObject(pen_brush);
                    }
                    let tx = b.right + s(10);
                    text(dc, body, TEXT, "Also ask every other app using the GPU to close", RECT { left: tx, top: l.check.top + s(7), right: l.check.right, bottom: l.check.top + s(27) }, DT_SINGLELINE | DT_END_ELLIPSIS);
                    let would = if p.preview.close.is_empty() {
                        "No other app with a window is using the GPU right now.".to_string()
                    } else {
                        format!("Would ask {}. Like clicking their X: they can still save.", list(&p.preview.close, 3))
                    };
                    text(dc, small, SUBTLE, &would, RECT { left: tx, top: l.check.top + s(27), right: l.check.right, bottom: l.check.bottom }, DT_SINGLELINE | DT_END_ELLIPSIS);
                }
                let restarts = if p.preview.restart.is_empty() {
                    "No browser or Electron app is using the GPU right now.".to_string()
                } else {
                    format!("Restarts the GPU process of {}, so their tabs and windows stay open.", list(&p.preview.restart, 4))
                };
                let note = format!("{restarts} Limits and cooldowns don't apply. Windows itself is never touched.");
                let note = note.as_str();
                let note_top = s(502);
                let note_h = measure(small, note, right - pad);
                text(dc, small, SUBTLE, note, RECT { left: pad, top: note_top, right, bottom: note_top + note_h }, DT_WORDBREAK);

                // Cancel / Close.
                let label = if matches!(p.stage, Stage::Done(_)) { "Close" } else { "Cancel" };
                fill(dc, l.cancel, if p.hover == Spot::Cancel { HOVER_BG } else { RULE });
                text(dc, bold, TEXT, label, l.cancel, DT_SINGLELINE | DT_CENTER | DT_VCENTER);

                // BAM: a white flash that fades out.
                if let Stage::Bam(t) = p.stage {
                    let e = ((now - t).as_secs_f32() / BAM.as_secs_f32()).clamp(0.0, 1.0);
                    let alpha = ((1.0 - e).powi(2) * 200.0) as u8;
                    flash(dc, client, alpha);
                }
            });
        });
        let _ = EndPaint(hwnd, &ps);
    }
}

fn rgb_red() -> COLORREF {
    crate::notify::rgb(230, 60, 48)
}

unsafe fn tick_mark(dc: HDC, b: RECT, width: i32) {
    let pen = CreatePen(PS_SOLID, width, TEXT);
    let old = SelectObject(dc, pen);
    let (w, h) = (b.right - b.left, b.bottom - b.top);
    let _ = MoveToEx(dc, b.left + w * 22 / 100, b.top + h * 52 / 100, None);
    let _ = LineTo(dc, b.left + w * 42 / 100, b.top + h * 72 / 100);
    let _ = LineTo(dc, b.left + w * 78 / 100, b.top + h * 30 / 100);
    SelectObject(dc, old);
    let _ = DeleteObject(pen);
}

/// Washes the whole panel with white at `alpha`.
unsafe fn flash(dc: HDC, client: RECT, alpha: u8) {
    let src = CreateCompatibleDC(dc);
    let bmp = CreateCompatibleBitmap(dc, 1, 1);
    let old = SelectObject(src, bmp);
    SetPixel(src, 0, 0, crate::notify::rgb(255, 255, 255));
    let f = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: 0 };
    let _ = AlphaBlend(dc, 0, 0, client.right, client.bottom, src, 0, 0, 1, 1, f);
    SelectObject(src, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(src);
}

#[cfg(test)]
mod tests {
    use super::list;

    #[test]
    fn lists_read_like_english() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(list(&names(&[]), 3), "");
        assert_eq!(list(&names(&["Opera"]), 3), "Opera");
        assert_eq!(list(&names(&["Opera", "Steam"]), 3), "Opera and Steam");
        assert_eq!(list(&names(&["Opera", "Steam", "Claude"]), 3), "Opera, Steam and Claude");
        assert_eq!(list(&names(&["Opera", "Steam", "Claude", "Discord", "Zoom"]), 3), "Opera, Steam, Claude and 2 more");
    }
}