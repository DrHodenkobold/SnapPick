//! The always-on core process: hidden message window, tray icon, hotkeys, floating button,
//! toast/flash feedback and the screenshot pipeline. It has no dependency on Explorer being
//! alive (the tray icon re-registers itself when the taskbar is recreated) and no dependency
//! on the settings UI, which is a separate process.

use crate::capture::{self, WinInfo};
use crate::config::{log, Config};
use crate::input::{self, WM_SHOOT, WM_TOGGLE};
use crate::watchdog;
use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::ExitProcess;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_RELOAD: u32 = WM_APP + 1;
const WM_FEEDBACK: u32 = WM_APP + 4;
const WM_TRAY: u32 = WM_APP + 10;

const ID_OPEN: usize = 1;
const ID_TOGGLE: usize = 2;
const ID_SHOOT: usize = 3;
const ID_QUIT: usize = 4;

const TIMER_HOUSEKEEPING: usize = 1;
const TIMER_TOAST: usize = 3;
const TIMER_FLASH: usize = 4;

const BTN_SIZE: i32 = 56;

struct Feedback {
    text: String,
    ok: bool,
    rect: Option<RECT>,
}
static FEEDBACK: Mutex<Option<Feedback>> = Mutex::new(None);

#[derive(Default)]
struct State {
    cfg: Config,
    core: HWND,
    button: HWND,
    toast: HWND,
    flash: HWND,
    toast_text: String,
    taskbar_created: u32,
    button_pos: Option<(i32, i32)>,
    drag_origin: POINT,
    drag_win: POINT,
    dragging: bool,
    ticks: u32,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    ST.with(|s| f(&mut s.borrow_mut()))
}

pub fn run(restarted: bool, background: bool) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();

        register(hinst, w!("SnapPickCoreWnd"), Some(core_proc), None);
        register(hinst, w!("SnapPickButton"), Some(button_proc), Some(CreateSolidBrush(COLORREF(0x00_30_30_30))));
        register(hinst, w!("SnapPickToast"), Some(toast_proc), Some(CreateSolidBrush(COLORREF(0x00_26_22_20))));
        register(hinst, w!("SnapPickFlash"), Some(flash_proc), Some(CreateSolidBrush(COLORREF(0x00_FF_FF_FF))));

        // Plain top-level (never shown) so it receives the TaskbarCreated broadcast.
        let core = CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("SnapPickCoreWnd"), w!("SnapPickCore"), WS_OVERLAPPED,
            0, 0, 0, 0, None, None, hinst, None,
        ).expect("core window");

        let tool = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
        let button = CreateWindowExW(
            tool, w!("SnapPickButton"), w!("SnapPickButton"), WS_POPUP,
            0, 0, BTN_SIZE, BTN_SIZE, None, None, hinst, None,
        ).expect("button window");
        SetWindowRgn(button, CreateEllipticRgn(0, 0, BTN_SIZE + 1, BTN_SIZE + 1), true);
        let toast = CreateWindowExW(
            tool, w!("SnapPickToast"), w!("SnapPickToast"), WS_POPUP,
            0, 0, 340, 56, None, None, hinst, None,
        ).expect("toast window");
        SetWindowRgn(toast, CreateRoundRectRgn(0, 0, 341, 57, 16, 16), true);
        let flash = CreateWindowExW(
            tool | WS_EX_LAYERED | WS_EX_TRANSPARENT, w!("SnapPickFlash"), w!("SnapPickFlash"), WS_POPUP,
            0, 0, 10, 10, None, None, hinst, None,
        ).expect("flash window");
        let _ = SetLayeredWindowAttributes(flash, COLORREF(0), 110, LWA_ALPHA);

        let cfg = Config::load();
        with(|s| {
            s.core = core;
            s.button = button;
            s.toast = toast;
            s.flash = flash;
            s.taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            s.cfg = cfg;
        });

        log(&format!("core: started pid={} restarted={restarted}", std::process::id()));
        tray_add(core);
        apply_config();
        SetTimer(core, TIMER_HOUSEKEEPING, 2000, None);

        if restarted {
            post_feedback("SnapPick recovered and restarted", true, None);
        } else if !background {
            open_ui();
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn register(hinst: HINSTANCE, name: PCWSTR, proc: WNDPROC, bg: Option<HBRUSH>) {
    let wc = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: hinst,
        lpszClassName: name,
        hbrBackground: bg.unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
}

// ---------------------------------------------------------------- config / lifecycle

fn reload_config() {
    let cfg = Config::load();
    with(|s| s.cfg = cfg);
    apply_config();
}

fn apply_config() {
    let (cfg, core) = with(|s| (s.cfg.clone(), s.core));
    input::set_bindings(&cfg.toggle_bind, &cfg.shoot_bind);
    if cfg.safe_mode {
        input::stop_hook();
        input::register_hotkeys(core, &cfg.toggle_bind, &cfg.shoot_bind);
    } else {
        input::unregister_hotkeys(core);
        input::start_hook(core);
    }
    set_autostart(cfg.autostart);
    watchdog::sync(&cfg);
    if !cfg.floating_button {
        hide_button();
    }
}

fn set_autostart(on: bool) {
    use winreg::{enums::*, RegKey};
    let run = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_SET_VALUE);
    if let Ok(run) = run {
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let _ = run.set_value("SnapPick", &format!("\"{}\" --background", exe.display()));
            }
        } else {
            let _ = run.delete_value("SnapPick");
        }
    }
}

fn quit() -> ! {
    unsafe {
        let core = with(|s| s.core);
        tray_remove(core);
        watchdog::stop_watchdog();
        ExitProcess(0)
    }
}

pub fn open_ui() {
    unsafe {
        if let Ok(h) = FindWindowW(None, w!("SnapPick")) {
            if !h.is_invalid() {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
                return;
            }
        }
    }
    watchdog::spawn_self(&["--ui"]);
}

// ---------------------------------------------------------------- tray

unsafe fn tray_data(core: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: core,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: make_icon(),
        ..Default::default()
    };
    for (i, c) in "SnapPick".encode_utf16().enumerate() {
        nid.szTip[i] = c;
    }
    nid
}

unsafe fn tray_add(core: HWND) {
    let nid = tray_data(core);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        log("tray: NIM_ADD failed (explorer not running?); will retry on TaskbarCreated");
    }
}

unsafe fn tray_remove(core: HWND) {
    let nid = tray_data(core);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// Red record-dot icon, drawn procedurally so no resource file is needed.
unsafe fn make_icon() -> HICON {
    const N: i32 = 32;
    let mut px = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            let (r, g, b, a) = if d <= 7.0 {
                (229, 72, 77, 255)
            } else if d <= 10.0 {
                (255, 255, 255, 255)
            } else if d <= 15.0 {
                (40, 40, 44, 255)
            } else {
                (0, 0, 0, 0)
            };
            let i = ((y * N + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[b, g, r, a]);
        }
    }
    let color = CreateBitmap(N, N, 1, 32, Some(px.as_ptr() as *const _));
    let mask = CreateBitmap(N, N, 1, 1, None);
    let ii = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&ii).unwrap_or_default();
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}

unsafe fn show_tray_menu(core: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let visible = with(|s| s.button_pos.is_some() && IsWindowVisible(s.button).as_bool());
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, &HSTRING::from("Open SnapPick"));
    let _ = AppendMenuW(
        menu, MF_STRING, ID_TOGGLE,
        &HSTRING::from(if visible { "Hide screenshot button" } else { "Show screenshot button" }),
    );
    let _ = AppendMenuW(menu, MF_STRING, ID_SHOOT, &HSTRING::from("Take screenshot now"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, &HSTRING::from("Quit SnapPick"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(core); // required or the menu never dismisses
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, core, None);
    let _ = PostMessageW(core, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

// ---------------------------------------------------------------- window procs

unsafe extern "system" fn core_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| core_proc_inner(hwnd, msg, wp, lp))) {
        Ok(r) => r,
        Err(_) => {
            log("panic in core_proc (recovered)");
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

unsafe fn core_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != 0 && msg == with(|s| s.taskbar_created) {
        tray_add(hwnd);
        return LRESULT(0);
    }
    match msg {
        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_tray_menu(hwnd),
                WM_LBUTTONUP => open_ui(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wp.0 & 0xFFFF {
                ID_OPEN => open_ui(),
                ID_TOGGLE => toggle_button(),
                ID_SHOOT => shoot(),
                ID_QUIT => quit(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            if wp.0 == 1 { toggle_button() } else { shoot() }
            LRESULT(0)
        }
        WM_TOGGLE => { toggle_button(); LRESULT(0) }
        WM_SHOOT => { shoot(); LRESULT(0) }
        WM_RELOAD => { reload_config(); LRESULT(0) }
        WM_FEEDBACK => { show_feedback(); LRESULT(0) }
        WM_TIMER if wp.0 == TIMER_HOUSEKEEPING => { housekeeping(); LRESULT(0) }
        WM_ENDSESSION => {
            if wp.0 != 0 { ExitProcess(0) }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Self-healing: respawn a missing watchdog, keep the hook alive, keep the button on top.
unsafe fn housekeeping() {
    let (cfg, core, button) = with(|s| {
        s.ticks += 1;
        (s.cfg.clone(), s.core, s.button)
    });
    watchdog::sync(&cfg);
    if !cfg.safe_mode {
        // Windows silently drops a low-level hook that ever times out; re-arm it every ~60 s.
        if with(|s| s.ticks % 30 == 0) {
            input::stop_hook();
        }
        input::start_hook(core);
    }
    if IsWindowVisible(button).as_bool() {
        let _ = SetWindowPos(button, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

// ---------------------------------------------------------------- floating button

fn toggle_button() {
    unsafe {
        let (cfg, button) = with(|s| (s.cfg.clone(), s.button));
        if !cfg.floating_button {
            post_feedback("Floating button is disabled in settings", false, None);
            return;
        }
        if IsWindowVisible(button).as_bool() {
            hide_button();
        } else {
            let (x, y) = with(|s| s.button_pos).unwrap_or_else(|| {
                let mut wa = RECT::default();
                let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
                (wa.right - BTN_SIZE - 40, wa.top + 120)
            });
            with(|s| s.button_pos = Some((x, y)));
            let _ = SetWindowPos(button, HWND_TOPMOST, x, y, BTN_SIZE, BTN_SIZE, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }
}

fn hide_button() {
    unsafe {
        let b = with(|s| s.button);
        let _ = ShowWindow(b, SW_HIDE);
    }
}

unsafe extern "system" fn button_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| button_proc_inner(hwnd, msg, wp, lp))) {
        Ok(r) => r,
        Err(_) => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn button_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let (red, white) = (CreateSolidBrush(COLORREF(0x00_4D_48_E5)), CreateSolidBrush(COLORREF(0x00_FF_FF_FF)));
            let old = SelectObject(dc, white);
            let pen = SelectObject(dc, GetStockObject(NULL_PEN));
            let _ = Ellipse(dc, 4, 4, BTN_SIZE - 4, BTN_SIZE - 4);
            SelectObject(dc, red);
            let _ = Ellipse(dc, 11, 11, BTN_SIZE - 11, BTN_SIZE - 11);
            SelectObject(dc, old);
            SelectObject(dc, pen);
            let _ = DeleteObject(red);
            let _ = DeleteObject(white);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let mut p = POINT::default();
            let mut r = RECT::default();
            let _ = GetCursorPos(&mut p);
            let _ = GetWindowRect(hwnd, &mut r);
            with(|s| {
                s.drag_origin = p;
                s.drag_win = POINT { x: r.left, y: r.top };
                s.dragging = false;
            });
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE if wp.0 & 1 != 0 => {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let (o, w0, was) = with(|s| (s.drag_origin, s.drag_win, s.dragging));
            let (dx, dy) = (p.x - o.x, p.y - o.y);
            if was || dx.abs() > 5 || dy.abs() > 5 {
                with(|s| s.dragging = true);
                let _ = SetWindowPos(hwnd, None, w0.x + dx, w0.y + dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            if with(|s| s.dragging) {
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                with(|s| s.button_pos = Some((r.left, r.top)));
            } else {
                shoot();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            hide_button();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------- toast / flash

fn post_feedback(text: &str, ok: bool, rect: Option<RECT>) {
    *FEEDBACK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Feedback { text: text.into(), ok, rect });
    unsafe {
        let core = with(|s| s.core);
        let _ = PostMessageW(core, WM_FEEDBACK, WPARAM(0), LPARAM(0));
    }
}

unsafe fn show_feedback() {
    let Some(fb) = FEEDBACK.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let (cfg, toast, flash) = with(|s| (s.cfg.clone(), s.toast, s.flash));

    if cfg.sound {
        let _ = MessageBeep(if fb.ok { MB_ICONASTERISK } else { MB_ICONHAND });
    }
    if cfg.flash && fb.ok {
        if let Some(r) = fb.rect {
            let _ = SetWindowPos(flash, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            SetTimer(flash, TIMER_FLASH, 110, None);
        }
    }
    // Errors always get a toast; a silent failure would be worse than ignoring the toggle.
    if cfg.toast || !fb.ok {
        with(|s| s.toast_text = fb.text.clone());
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let _ = SetWindowPos(toast, HWND_TOPMOST, wa.right - 340 - 20, wa.bottom - 56 - 20, 340, 56, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = InvalidateRect(toast, None, true);
        SetTimer(toast, TIMER_TOAST, 2200, None);
    }
}

unsafe extern "system" fn toast_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let text = with(|s| s.toast_text.clone());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00_FF_FF_FF));
            SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
            let mut r = RECT { left: 16, top: 0, right: 324, bottom: 56 };
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(dc, &mut wide, &mut r, DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_TOAST => {
            let _ = KillTimer(hwnd, TIMER_TOAST);
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn flash_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == TIMER_FLASH => {
            let _ = KillTimer(hwnd, TIMER_FLASH);
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------- screenshot pipeline

fn shoot() {
    let cfg = with(|s| s.cfg.clone());
    std::thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| do_shoot(&cfg)))
            .unwrap_or_else(|_| Err("Internal error while capturing".into()));
        match result {
            Ok((text, rect)) => {
                log(&format!("shot ok: {text}"));
                post_feedback(&text, true, rect)
            }
            Err(e) => {
                log(&format!("shot failed: {e}"));
                post_feedback(&e, false, None)
            }
        }
    });
}

fn resolve(t: &crate::config::Target) -> Result<WinInfo, String> {
    let all = capture::list_windows();
    let found = all
        .iter()
        .find(|w| w.hwnd == t.hwnd && w.exe == t.exe)
        .or_else(|| all.iter().find(|w| w.exe == t.exe && w.title == t.title))
        .or_else(|| all.iter().find(|w| !t.exe.is_empty() && w.exe == t.exe))
        .ok_or_else(|| format!("\"{}\" is not open any more", t.title))?;
    Ok(found.clone())
}

fn do_shoot(cfg: &Config) -> Result<(String, Option<RECT>), String> {
    let t = cfg.target.as_ref().ok_or("No window selected - open SnapPick and pick one")?;
    let win = resolve(t)?;
    if win.minimized {
        return Err(format!("\"{}\" is minimized - restore it first", win.title));
    }
    let rect = capture::frame_rect(win.hwnd);
    let img = capture::capture(win.hwnd)?;

    let mut last_err = String::new();
    let mut copied = false;
    for _ in 0..6 {
        let data = arboard::ImageData {
            width: img.width() as usize,
            height: img.height() as usize,
            bytes: std::borrow::Cow::Borrowed(img.as_raw()),
        };
        match arboard::Clipboard::new().and_then(|mut c| c.set_image(data)) {
            Ok(()) => { copied = true; break; }
            Err(e) => { last_err = e.to_string(); std::thread::sleep(std::time::Duration::from_millis(80)); }
        }
    }
    if !copied {
        return Err(format!("Clipboard busy: {last_err}"));
    }

    let mut msg = String::from("Screenshot copied to clipboard");
    if cfg.save_to_folder {
        let dir = std::path::PathBuf::from(&cfg.save_folder);
        let t = unsafe { GetLocalTime() };
        let name = format!(
            "SnapPick_{:04}{:02}{:02}_{:02}{:02}{:02}_{:03}.png",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        );
        let saved = std::fs::create_dir_all(&dir).is_ok() && img.save(dir.join(&name)).is_ok();
        msg = if saved { "Copied and saved".into() } else { "Copied (saving to folder failed)".into() };
    }
    Ok((msg, rect))
}
