//! Guard prompt and OSD. They are layered, topmost and never activated, so the game in front keeps
//! input; they are not toasts either, because Focus Assist would swallow those.

use crate::guard::Side;
use crate::i18n::{Key, ResolvedLang, fmt, t};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetMonitorInfoW,
    HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    MonitorFromWindow, ScreenToClient, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow, IDC_ARROW,
    IDC_HAND, KillTimer, LoadCursorW, MA_NOACTIVATE, PostMessageW, RegisterClassExW,
    SW_SHOWNOACTIVATE, SetCursor, SetTimer, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WINDOW_EX_STYLE, WM_APP, WM_CAPTURECHANGED, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_SETCURSOR, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, Result, w};

/// wparam carries the `Choice` index; lparam is 0 for output and 1 for input.
pub const WM_APP_PROMPT_CHOICE: u32 = WM_APP + 2;

const WINDOW_CLASS: PCWSTR = w!("SwaraPopup");
const FACE: PCWSTR = w!("Segoe UI");
const HIDE_TIMER_ID: usize = 1;
const OSD_VISIBLE_MS: u32 = 1500;
const NO_ACTIVATE: LRESULT = LRESULT(MA_NOACTIVATE as isize);
const MONITORINFO_BYTES: u32 = size_of::<MONITORINFO>() as u32;
const CLASS_BYTES: u32 = size_of::<WNDCLASSEXW>() as u32;
const HEADER_BYTES: u32 = size_of::<BITMAPINFOHEADER>() as u32;

const SURFACE: D2D1_COLOR_F = hex(0x0c0d0e);
const OUTLINE: D2D1_COLOR_F = hex(0x2c2f33);
const INK: D2D1_COLOR_F = hex(0xfafaf9);
const DIM_INK: D2D1_COLOR_F = hex(0xb8b3ad);
const ACCENT: D2D1_COLOR_F = hex(0x0f766e);
const ON_ACCENT: D2D1_COLOR_F = hex(0xffffff);
const KEY_FILL: D2D1_COLOR_F = hex(0x22262a);

type Area = D2D_RECT_F;
type Align = DWRITE_TEXT_ALIGNMENT;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    SwitchBack = 0,
    Keep = 1,
    Snooze = 2,
}

impl Choice {
    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::SwitchBack),
            1 => Some(Self::Keep),
            2 => Some(Self::Snooze),
            _ => None,
        }
    }
}

/// One drifted device: the label for the current one, the device Windows picked, and the user's.
pub struct PromptRow {
    pub now_label: Key,
    pub now: String,
    pub pick: String,
}

#[derive(Clone, Copy)]
struct OpenPrompt {
    hwnd: HWND,
    notify: HWND,
    side_code: isize,
    keys: [Area; 3],
    /// Index pressed with the left button, so press and release must hit the same key.
    held: Option<usize>,
}

thread_local! {
    static OPEN_PROMPT: Cell<Option<OpenPrompt>> = const { Cell::new(None) };
    static OPEN_OSD: Cell<Option<HWND>> = const { Cell::new(None) };
}

const fn channel(v: u32, shift: u32) -> f32 {
    ((v >> shift) & 0xff) as f32 / 255.0
}

const fn hex(v: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: channel(v, 16),
        g: channel(v, 8),
        b: channel(v, 0),
        a: 1.0,
    }
}

/// Half-open on the right and bottom: two keys sharing an edge never both match.
fn key_at(keys: &[Area], x: f32, y: f32) -> Option<usize> {
    keys.iter()
        .position(|k| (k.left..k.right).contains(&x) && (k.top..k.bottom).contains(&y))
}

/// Dragging from the game onto the prompt must not choose anything, so only a release on the
/// pressed key counts.
fn clicked(held: Option<usize>, released: Option<usize>) -> Option<usize> {
    held.filter(|&k| released == Some(k))
}

fn current_prompt(hwnd: HWND) -> Option<OpenPrompt> {
    OPEN_PROMPT.with(Cell::get).filter(|p| p.hwnd == hwnd)
}

fn key_under(hwnd: HWND, x: f32, y: f32) -> Option<(OpenPrompt, usize)> {
    let prompt = current_prompt(hwnd)?;
    key_at(&prompt.keys, x, y).map(|k| (prompt, k))
}

fn client_point(lparam: LPARAM) -> (f32, f32) {
    let low = lparam.0 as i16;
    let high = (lparam.0 >> 16) as i16;
    (f32::from(low), f32::from(high))
}

unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => NO_ACTIVATE,
        WM_SETCURSOR => {
            let mut cursor = POINT::default();
            // SAFETY: `cursor` is a live local and `hwnd` is the window being asked about.
            let mapped = unsafe {
                GetCursorPos(&mut cursor).is_ok() && ScreenToClient(hwnd, &mut cursor).as_bool()
            };
            let on_key = mapped && key_under(hwnd, cursor.x as f32, cursor.y as f32).is_some();
            let shape = if on_key { IDC_HAND } else { IDC_ARROW };
            // SAFETY: shared system cursor; nothing to free.
            unsafe { SetCursor(LoadCursorW(None, shape).ok()) };
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = client_point(lparam);
            let key = key_under(hwnd, x, y).map(|(_, k)| k);
            mark_held(hwnd, key);
            if key.is_some() {
                // SAFETY: capture on our own window so the release is delivered here.
                unsafe { SetCapture(hwnd) };
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = client_point(lparam);
            let held = current_prompt(hwnd).and_then(|p| p.held);
            mark_held(hwnd, None);
            if held.is_some() {
                // SAFETY: undoes the SetCapture from WM_LBUTTONDOWN.
                let _ = unsafe { ReleaseCapture() };
            }
            if let Some((prompt, under)) = key_under(hwnd, x, y)
                && let Some(key) = clicked(held, Some(under))
            {
                // SAFETY: plain post; if the owner is gone the call just fails.
                let _ = unsafe {
                    PostMessageW(
                        Some(prompt.notify),
                        WM_APP_PROMPT_CHOICE,
                        WPARAM(key),
                        LPARAM(prompt.side_code),
                    )
                };
                hide_prompt();
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            mark_held(hwnd, None);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == HIDE_TIMER_ID => {
            hide_osd();
            LRESULT(0)
        }
        // Clear our slot here too, for when the window dies by some other route.
        WM_DESTROY => {
            OPEN_PROMPT.with(|slot| {
                if slot.get().is_some_and(|p| p.hwnd == hwnd) {
                    slot.set(None);
                }
            });
            OPEN_OSD.with(|slot| {
                if slot.get() == Some(hwnd) {
                    slot.set(None);
                }
            });
            LRESULT(0)
        }
        // SAFETY: unhandled message passed through as received.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn mark_held(hwnd: HWND, key: Option<usize>) {
    if let Some(mut prompt) = current_prompt(hwnd) {
        prompt.held = key;
        OPEN_PROMPT.with(|slot| slot.set(Some(prompt)));
    }
}

fn module() -> HINSTANCE {
    // SAFETY: a null name returns the exe's own module, valid for the process lifetime.
    let handle = unsafe { GetModuleHandleW(PCWSTR::null()) };
    handle.map_or(HINSTANCE::default(), |m| HINSTANCE(m.0))
}

/// Work area of `screen` and its scale factor relative to 96 dpi.
fn work_area(screen: HMONITOR) -> (RECT, f32) {
    let mut info = MONITORINFO {
        cbSize: MONITORINFO_BYTES,
        ..Default::default()
    };
    let mut dpi = (96u32, 96u32);
    // SAFETY: `screen` is a monitor handle and every out pointer is a live local.
    unsafe {
        let _ = GetMonitorInfoW(screen, &mut info);
        let _ = GetDpiForMonitor(screen, MDT_EFFECTIVE_DPI, &mut dpi.0, &mut dpi.1);
    }
    (info.rcWork, dpi.0 as f32 / 96.0)
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

thread_local! {
    /// Made by the first popup, not at start, then kept: building them again cost every OSD the
    /// cold Direct2D and font setup. Single-threaded factory, so one per thread.
    static FACTORIES: RefCell<Option<(ID2D1Factory, IDWriteFactory)>> = const { RefCell::new(None) };
}

/// A failed attempt is not kept, so the next popup tries again.
fn factories() -> Result<(ID2D1Factory, IDWriteFactory)> {
    FACTORIES.with_borrow_mut(|slot| {
        if let Some(made) = slot {
            return Ok(made.clone());
        }
        // SAFETY: plain factory creation; the interfaces own their references.
        let made = unsafe {
            (
                D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED)?,
            )
        };
        *slot = Some(made.clone());
        Ok(made)
    })
}

/// Render target plus text factory for one popup, released on drop; the factories stay cached.
struct Painter {
    target: ID2D1DCRenderTarget,
    words: IDWriteFactory,
}

impl Painter {
    fn new() -> Result<Self> {
        // Pinned at 96 dpi: callers already scale every size, so one DIP equals one pixel.
        let target_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let (factory, words) = factories()?;
        // SAFETY: target creation with a fully built properties struct.
        unsafe {
            let target = factory.CreateDCRenderTarget(&target_props)?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Ok(Self { target, words })
        }
    }

    /// One line, vertically centred, cut with an ellipsis when too wide.
    fn font(&self, size: f32, bold: bool, align: Align) -> Result<IDWriteTextFormat> {
        let weight = if bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        };
        let cut = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            ..Default::default()
        };
        // SAFETY: constant face and locale strings; setters run on the format just created.
        unsafe {
            let format = self.words.CreateTextFormat(
                FACE,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )?;
            format.SetTextAlignment(align)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let ellipsis = self.words.CreateEllipsisTrimmingSign(&format)?;
            format.SetTrimming(&cut, &ellipsis)?;
            Ok(format)
        }
    }

    fn measure(&self, s: &str, format: &IDWriteTextFormat) -> Result<f32> {
        let chars = utf16(s);
        let mut metrics = DWRITE_TEXT_METRICS::default();
        // SAFETY: `chars` and `metrics` stay alive across both calls.
        unsafe {
            let layout = self
                .words
                .CreateTextLayout(&chars, format, 10_000.0, 1_000.0)?;
            layout.GetMetrics(&mut metrics)?;
        }
        Ok(metrics.widthIncludingTrailingWhitespace.ceil())
    }

    fn label(
        &self,
        s: &str,
        format: &IDWriteTextFormat,
        color: &D2D1_COLOR_F,
        at: Area,
    ) -> Result<()> {
        let chars = utf16(s);
        // SAFETY: only used inside the BeginDraw/EndDraw pair in `paint`.
        unsafe {
            let brush = self.target.CreateSolidColorBrush(color, None)?;
            self.target.DrawText(
                &chars,
                format,
                &at,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
        Ok(())
    }

    fn panel(
        &self,
        at: Area,
        corner: f32,
        fill: &D2D1_COLOR_F,
        stroke: Option<&D2D1_COLOR_F>,
    ) -> Result<()> {
        let outline = D2D1_ROUNDED_RECT {
            rect: at,
            radiusX: corner,
            radiusY: corner,
        };
        // SAFETY: only used inside the BeginDraw/EndDraw pair in `paint`.
        unsafe {
            let fill_brush = self.target.CreateSolidColorBrush(fill, None)?;
            self.target.FillRoundedRectangle(&outline, &fill_brush);
            if let Some(color) = stroke {
                let stroke_brush = self.target.CreateSolidColorBrush(color, None)?;
                self.target
                    .DrawRoundedRectangle(&outline, &stroke_brush, 1.0, None);
            }
        }
        Ok(())
    }

    /// Makes a hidden layered window, draws into it, then reveals it without activation.
    fn open(
        &self,
        ex_style: WINDOW_EX_STYLE,
        origin: POINT,
        dims: (i32, i32),
        draw: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<HWND> {
        // Not a Once: a failed registration is tried again by the next popup.
        static CLASS_READY: AtomicBool = AtomicBool::new(false);
        if !CLASS_READY.load(Ordering::Relaxed) {
            let class = WNDCLASSEXW {
                cbSize: CLASS_BYTES,
                lpfnWndProc: Some(popup_proc),
                hInstance: module(),
                lpszClassName: WINDOW_CLASS,
                ..Default::default()
            };
            // SAFETY: `class` is complete and `popup_proc` is a static function.
            if unsafe { RegisterClassExW(&class) } != 0 {
                CLASS_READY.store(true, Ordering::Relaxed);
            } else {
                let why = windows::core::Error::from_thread();
                crate::log::error(&format!("popup class not registered: {why}"));
            }
        }
        // SAFETY: our registered class, top-level; destroyed below if painting fails.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                WINDOW_CLASS,
                PCWSTR::null(),
                WS_POPUP,
                origin.x,
                origin.y,
                dims.0,
                dims.1,
                None,
                None,
                Some(module()),
                None,
            )?
        };
        match self.paint(hwnd, origin, dims, draw) {
            Ok(()) => Ok(hwnd),
            Err(e) => {
                // SAFETY: freshly created and not yet stored in any slot.
                let _ = unsafe { DestroyWindow(hwnd) };
                Err(e)
            }
        }
    }

    fn paint(
        &self,
        hwnd: HWND,
        origin: POINT,
        (width, height): (i32, i32),
        draw: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        let header = BITMAPINFOHEADER {
            biSize: HEADER_BYTES,
            biWidth: width,
            // Negative height makes row 0 the top row.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as _,
            BlendFlags: 0,
            SourceConstantAlpha: u8::MAX,
            AlphaFormat: AC_SRC_ALPHA as _,
        };
        let bounds = RECT {
            right: width,
            bottom: height,
            ..Default::default()
        };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        // SAFETY: the memory DC and DIB are created and freed in this block whatever happens.
        unsafe {
            let mem_dc = CreateCompatibleDC(None);
            let mut pixels: *mut c_void = std::ptr::null_mut();
            let dib =
                match CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0) {
                    Ok(dib) => dib,
                    Err(e) => {
                        let _ = DeleteDC(mem_dc);
                        return Err(e);
                    }
                };
            let previous = SelectObject(mem_dc, dib.into());
            let result = (|| {
                self.target.BindDC(mem_dc, &bounds)?;
                self.target.BeginDraw();
                self.target.Clear(Some(&D2D1_COLOR_F::default()));
                let drawn = draw(self);
                self.target.EndDraw(None, None)?;
                drawn?;
                UpdateLayeredWindow(
                    hwnd,
                    None,
                    Some(&origin),
                    Some(&size),
                    Some(mem_dc),
                    Some(&POINT::default()),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                )?;
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                Ok(())
            })();
            SelectObject(mem_dc, previous);
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(mem_dc);
            result
        }
    }
}

/// Opens the guard prompt in the bottom-right corner of the primary monitor, closing any old one.
pub fn show_prompt(
    owner: HWND,
    lang: ResolvedLang,
    side: Side,
    rows: &[PromptRow],
    snooze_min: u32,
) -> Result<()> {
    hide_prompt();
    // The primary monitor is the one containing (0, 0); the cursor position doesn't matter.
    // SAFETY: monitor lookup by value.
    let screen = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    let (work, scale) = work_area(screen);
    let px = |v: f32| (v * scale).round();
    let painter = Painter::new()?;
    let heading_font = painter.font(px(13.0), true, DWRITE_TEXT_ALIGNMENT_LEADING)?;
    let name_font = painter.font(px(12.0), false, DWRITE_TEXT_ALIGNMENT_LEADING)?;
    let device_font = painter.font(px(12.0), true, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
    let key_font = painter.font(px(12.0), true, DWRITE_TEXT_ALIGNMENT_CENTER)?;

    let heading = match side {
        Side::Out => t(lang, Key::PopupOutputChanged),
        Side::In => t(lang, Key::PopupInputChanged),
    };
    let minutes = snooze_min.to_string();
    let captions = [
        t(lang, Key::PopupRevert).to_string(),
        t(lang, Key::PopupKeep).to_string(),
        fmt(t(lang, Key::PopupSnooze), &[("n", &minutes)]),
    ];
    let mut entries: Vec<(&str, &str)> = Vec::with_capacity(rows.len() * 2);
    for row in rows {
        entries.push((t(lang, row.now_label), &row.now));
        entries.push((t(lang, Key::PopupYourPick), &row.pick));
    }

    let margin = px(12.0);
    let spacing = px(4.0);
    let heading_h = px(18.0);
    let entry_h = px(20.0);
    let key_h = px(28.0);
    let mut key_w = px(64.0);
    for caption in &captions {
        key_w = key_w.max(painter.measure(caption, &key_font)? + px(16.0));
    }
    let mut width = px(260.0).max(3.0 * key_w + 2.0 * spacing + 2.0 * margin);
    for (name, device) in &entries {
        let wanted = painter.measure(name, &name_font)?
            + px(16.0)
            + painter.measure(device, &device_font)?
            + 2.0 * margin;
        width = width.max(wanted.min(px(360.0)));
    }
    key_w = (width - 2.0 * margin - 2.0 * spacing) / 3.0;
    let list_top = margin + heading_h + px(6.0);
    let keys_top = list_top + entries.len() as f32 * entry_h + px(10.0);
    let height = keys_top + key_h + margin;
    let keys: [Area; 3] = std::array::from_fn(|i| {
        let left = margin + (key_w + spacing) * i as f32;
        Area {
            left,
            top: keys_top,
            right: left + key_w,
            bottom: keys_top + key_h,
        }
    });

    let dims = (width as i32, height as i32);
    let edge_gap = px(24.0) as i32;
    let origin = POINT {
        x: work.right - dims.0 - edge_gap,
        y: work.bottom - dims.1 - edge_gap,
    };
    let hwnd = painter.open(WINDOW_EX_STYLE(0), origin, dims, |p| {
        let frame = Area {
            left: 0.5,
            top: 0.5,
            right: width - 0.5,
            bottom: height - 0.5,
        };
        p.panel(frame, px(8.0), &SURFACE, Some(&OUTLINE))?;
        let band = |top: f32, h: f32| Area {
            left: margin,
            top,
            right: width - margin,
            bottom: top + h,
        };
        p.label(heading, &heading_font, &INK, band(margin, heading_h))?;
        for (n, (name, device)) in entries.iter().enumerate() {
            let top = list_top + n as f32 * entry_h;
            let name_w = p.measure(name, &name_font)?;
            p.label(name, &name_font, &DIM_INK, band(top, entry_h))?;
            let mut value_area = band(top, entry_h);
            value_area.left += name_w + px(16.0);
            p.label(device, &device_font, &INK, value_area)?;
        }
        for (n, (area, caption)) in keys.iter().zip(&captions).enumerate() {
            let primary = n == 0;
            let fill = if primary { &ACCENT } else { &KEY_FILL };
            let text = if primary { &ON_ACCENT } else { &INK };
            p.panel(*area, px(5.0), fill, None)?;
            p.label(caption, &key_font, text, *area)?;
        }
        Ok(())
    })?;
    let side_code = match side {
        Side::Out => 0,
        Side::In => 1,
    };
    OPEN_PROMPT.with(|slot| {
        slot.set(Some(OpenPrompt {
            hwnd,
            notify: owner,
            side_code,
            keys,
            held: None,
        }))
    });
    Ok(())
}

/// Flashes `text` near the top of the foreground window's monitor for 1.5 s, replacing any OSD
/// still up. Text after a tab is drawn dimmer as the hotkey hint. `_owner` only mirrors
/// `show_prompt`; nothing is posted back.
pub fn show_osd(_owner: HWND, text: &str) {
    hide_osd();
    if let Err(e) = open_osd(text) {
        crate::log::error(&format!("osd not shown: {e}"));
    }
}

fn open_osd(text: &str) -> Result<()> {
    // SAFETY: lookups only; with no foreground window this falls back to the nearest monitor.
    let screen = unsafe { MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST) };
    let (work, scale) = work_area(screen);
    let px = |v: f32| (v * scale).round();
    let painter = Painter::new()?;
    let device_font = painter.font(px(13.0), true, DWRITE_TEXT_ALIGNMENT_LEADING)?;
    let hint_font = painter.font(px(12.0), false, DWRITE_TEXT_ALIGNMENT_LEADING)?;
    let (device, hint) = text.split_once('\t').unwrap_or((text, ""));
    let margin = px(12.0);
    let spacing = px(8.0);
    let hint_w = match hint {
        "" => 0.0,
        h => painter.measure(h, &hint_font)?.min(px(160.0)),
    };
    let device_w = painter
        .measure(device, &device_font)?
        .min(px(480.0) - 2.0 * margin - spacing - hint_w)
        .max(0.0);
    let hint_span = if hint_w > 0.0 { spacing + hint_w } else { 0.0 };
    let width = 2.0 * margin + device_w + hint_span;
    let height = px(34.0);
    let dims = (width as i32, height as i32);
    let origin = POINT {
        x: work.left + (work.right - work.left - dims.0) / 2,
        y: work.top + px(48.0) as i32,
    };
    // Mouse passes through: nothing on the OSD is clickable.
    let hwnd = painter.open(WS_EX_TRANSPARENT, origin, dims, |p| {
        let frame = Area {
            left: 0.5,
            top: 0.5,
            right: width - 0.5,
            bottom: height - 0.5,
        };
        p.panel(frame, px(8.0), &SURFACE, Some(&OUTLINE))?;
        let column = |from: f32, to: f32| Area {
            left: from,
            top: 0.0,
            right: to,
            bottom: height,
        };
        p.label(
            device,
            &device_font,
            &INK,
            column(margin, margin + device_w),
        )?;
        if hint_w > 0.0 {
            let from = margin + device_w + spacing;
            p.label(hint, &hint_font, &DIM_INK, column(from, width))?;
        }
        Ok(())
    })?;
    OPEN_OSD.with(|slot| slot.set(Some(hwnd)));
    // SAFETY: `hwnd` is the OSD just opened; `hide_osd` kills this timer.
    let armed = unsafe { SetTimer(Some(hwnd), HIDE_TIMER_ID, OSD_VISIBLE_MS, None) } != 0;
    if !armed {
        let why = windows::core::Error::from_thread();
        // No timer means nothing would ever close it, so close it now.
        hide_osd();
        return Err(why);
    }
    Ok(())
}

/// Closes the guard prompt, if open. Call from the thread that opened it.
pub fn hide_prompt() {
    if let Some(prompt) = OPEN_PROMPT.with(Cell::take) {
        // SAFETY: our window; a handle that is already gone only makes this fail.
        let _ = unsafe { DestroyWindow(prompt.hwnd) };
    }
}

fn hide_osd() {
    let Some(hwnd) = OPEN_OSD.with(Cell::take) else {
        return;
    };
    // SAFETY: our window; both calls are harmless failures on a dead handle.
    unsafe {
        let _ = KillTimer(Some(hwnd), HIDE_TIMER_ID);
        let _ = DestroyWindow(hwnd);
    }
}

/// Closes both the prompt and the OSD. Call from the thread that opened them.
pub fn hide_all() {
    hide_prompt();
    hide_osd();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(left: f32, top: f32, right: f32, bottom: f32) -> Area {
        Area {
            left,
            top,
            right,
            bottom,
        }
    }

    fn pair() -> [Area; 2] {
        [area(4.0, 2.0, 40.0, 26.0), area(48.0, 2.0, 84.0, 26.0)]
    }

    #[test]
    fn point_inside_a_key_selects_it() {
        assert_eq!(key_at(&pair(), 12.0, 14.0), Some(0));
        assert_eq!(key_at(&pair(), 70.0, 9.0), Some(1));
    }

    #[test]
    fn point_between_or_beyond_keys_selects_nothing() {
        assert_eq!(key_at(&pair(), 44.0, 14.0), None);
        assert_eq!(key_at(&pair(), 12.0, 30.0), None);
        assert_eq!(key_at(&[], 1.0, 1.0), None);
    }

    #[test]
    fn shared_edge_goes_to_the_right_hand_key() {
        let touching = [area(4.0, 2.0, 40.0, 26.0), area(40.0, 2.0, 76.0, 26.0)];
        assert_eq!(key_at(&touching, 4.0, 2.0), Some(0));
        assert_eq!(key_at(&touching, 40.0, 14.0), Some(1));
        assert_eq!(key_at(&touching, 76.0, 14.0), None);
        assert_eq!(key_at(&touching, 12.0, 26.0), None);
    }

    #[test]
    fn press_and_release_on_one_key_is_a_click() {
        assert_eq!(clicked(Some(1), Some(1)), Some(1));
        assert_eq!(clicked(Some(2), Some(2)), Some(2));
    }

    #[test]
    fn mismatched_or_missing_press_is_not_a_click() {
        assert_eq!(clicked(Some(2), Some(0)), None);
        assert_eq!(clicked(Some(1), None), None);
        assert_eq!(clicked(None, Some(0)), None);
        assert_eq!(clicked(None, None), None);
    }

    #[test]
    fn choice_index_maps_back_to_the_same_choice() {
        let all = [Choice::SwitchBack, Choice::Keep, Choice::Snooze];
        for choice in all {
            assert_eq!(Choice::from_index(choice as usize), Some(choice));
        }
        assert_eq!(Choice::from_index(7), None);
    }
}
