//! Window enumeration and capture. PrintWindow with PW_RENDERFULLCONTENT asks DWM for the
//! window's own pixels, so covered windows capture correctly.

use image::RgbaImage;
use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const PW_RENDERFULLCONTENT: u32 = 2;

#[derive(Clone, Debug)]
pub struct WinInfo {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
    pub minimized: bool,
}

pub fn list_windows() -> Vec<WinInfo> {
    let mut out: Vec<WinInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<WinInfo>);
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
        return true.into();
    }
    if GetWindow(hwnd, GW_OWNER).map(|h| !h.is_invalid()).unwrap_or(false) {
        return true.into();
    }
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
    if cloaked != 0 {
        return true.into();
    }
    let len = GetWindowTextLengthW(hwnd);
    if len == 0 {
        return true.into();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..n as usize]);
    if title.is_empty() || title == "Program Manager" || title == "SnapPick" {
        return true.into();
    }
    out.push(WinInfo {
        hwnd: hwnd.0 as isize,
        title,
        exe: exe_name(hwnd),
        minimized: IsIconic(hwnd).as_bool(),
    });
    true.into()
}

fn exe_name(hwnd: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit('\\').next().unwrap_or("").to_string()
    }
}

/// Visible frame bounds in screen coordinates (excludes the invisible resize border).
pub fn frame_rect(hwnd: isize) -> Option<RECT> {
    unsafe {
        let h = HWND(hwnd as *mut _);
        let mut r = RECT::default();
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, 16).is_ok() {
            return Some(r);
        }
        GetWindowRect(h, &mut r).ok().map(|_| r)
    }
}

pub fn capture(hwnd: isize) -> Result<RgbaImage, String> {
    unsafe {
        let h = HWND(hwnd as *mut _);
        if !IsWindow(h).as_bool() {
            return Err("The selected window no longer exists".into());
        }
        if IsIconic(h).as_bool() {
            return Err("The selected window is minimized".into());
        }
        let mut wr = RECT::default();
        GetWindowRect(h, &mut wr).map_err(|e| e.to_string())?;
        let (w, hgt) = (wr.right - wr.left, wr.bottom - wr.top);
        if w <= 0 || hgt <= 0 {
            return Err("The selected window has no size".into());
        }

        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, hgt);
        let old = SelectObject(mem, bmp);

        let printed = PrintWindow(h, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();

        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -hgt, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * hgt * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, hgt as u32, Some(pixels.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);

        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        if !printed || lines == 0 {
            return Err("Windows would not render that window".into());
        }
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255;
        }
        let mut img = RgbaImage::from_raw(w as u32, hgt as u32, pixels).ok_or("bad bitmap")?;

        if let Some(fr) = frame_rect(hwnd) {
            let x = (fr.left - wr.left).clamp(0, w - 1) as u32;
            let y = (fr.top - wr.top).clamp(0, hgt - 1) as u32;
            let cw = ((fr.right - fr.left).max(1) as u32).min(w as u32 - x);
            let ch = ((fr.bottom - fr.top).max(1) as u32).min(hgt as u32 - y);
            if (x, y, cw, ch) != (0, 0, w as u32, hgt as u32) {
                img = image::imageops::crop_imm(&img, x, y, cw, ch).to_image();
            }
        }
        Ok(img)
    }
}
