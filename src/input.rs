//! Global input: either a low-level keyboard+mouse hook (any key or mouse button) or,
//! in safe mode, plain RegisterHotKey on the core window (keyboard only, no hook).

use crate::config::Binding;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering::Relaxed};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TOGGLE: u32 = WM_APP + 3;
pub const WM_SHOOT: u32 = WM_APP + 2;

/// Index 0 = toggle button, 1 = direct shot. Read lock-free by the hook callbacks.
static BINDS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static HELD: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static SWALLOWED: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static CORE_HWND: AtomicIsize = AtomicIsize::new(0);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

pub fn set_bindings(toggle: &Binding, shoot: &Binding) {
    BINDS[0].store(toggle.pack(), Relaxed);
    BINDS[1].store(shoot.pack(), Relaxed);
}

fn bit_get(set: &[AtomicU64; 4], vk: u32) -> bool {
    set[(vk >> 6) as usize].load(Relaxed) & (1 << (vk & 63)) != 0
}
fn bit_set(set: &[AtomicU64; 4], vk: u32, on: bool) {
    let m = 1u64 << (vk & 63);
    if on {
        set[(vk >> 6) as usize].fetch_or(m, Relaxed);
    } else {
        set[(vk >> 6) as usize].fetch_and(!m, Relaxed);
    }
}

fn mods_now() -> u64 {
    let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0;
    (down(VK_CONTROL) as u64) << 8
        | (down(VK_SHIFT) as u64) << 9
        | (down(VK_MENU) as u64) << 10
        | ((down(VK_LWIN) || down(VK_RWIN)) as u64) << 11
}

/// Returns true when the event must be swallowed.
fn handle(vk: u32, down: bool) -> bool {
    if vk > 255 {
        return false;
    }
    if !down {
        bit_set(&HELD, vk, false);
        if bit_get(&SWALLOWED, vk) {
            bit_set(&SWALLOWED, vk, false);
            return true;
        }
        return false;
    }
    let mods = mods_now();
    for (i, b) in BINDS.iter().enumerate() {
        let p = b.load(Relaxed);
        if p & (1 << 13) == 0 || p & 0xFF != vk as u64 || p & 0xF00 != mods {
            continue;
        }
        // A held key auto-repeats; fire once per physical press.
        if !bit_get(&HELD, vk) {
            bit_set(&HELD, vk, true);
            let msg = if i == 0 { WM_TOGGLE } else { WM_SHOOT };
            unsafe {
                let _ = PostMessageW(HWND(CORE_HWND.load(Relaxed) as *mut _), msg, WPARAM(0), LPARAM(0));
            }
        }
        if p & (1 << 12) != 0 {
            bit_set(&SWALLOWED, vk, true);
            return true;
        }
        return false;
    }
    false
}

unsafe extern "system" fn kbd_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if kb.flags.0 & LLKHF_INJECTED.0 == 0 {
            let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let up = matches!(wp.0 as u32, WM_KEYUP | WM_SYSKEYUP);
            if (down || up) && handle(kb.vkCode, down) {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let m = wp.0 as u32;
        if m != WM_MOUSEMOVE {
            let ms = &*(lp.0 as *const MSLLHOOKSTRUCT);
            let xb = if (ms.mouseData >> 16) & 0xFFFF == 1 { 5 } else { 6 };
            let ev = match m {
                WM_LBUTTONDOWN => Some((1, true)),
                WM_LBUTTONUP => Some((1, false)),
                WM_RBUTTONDOWN => Some((2, true)),
                WM_RBUTTONUP => Some((2, false)),
                WM_MBUTTONDOWN => Some((4, true)),
                WM_MBUTTONUP => Some((4, false)),
                WM_XBUTTONDOWN => Some((xb, true)),
                WM_XBUTTONUP => Some((xb, false)),
                _ => None,
            };
            if let Some((vk, down)) = ev {
                if ms.flags & 1 == 0 && handle(vk, down) {
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

pub fn start_hook(core: HWND) {
    if HOOK_THREAD.load(Relaxed) != 0 {
        return;
    }
    CORE_HWND.store(core.0 as isize, Relaxed);
    HOOK_THREAD.store(u32::MAX, Relaxed); // claimed; real id set by the thread
    std::thread::spawn(|| unsafe {
        let hmod = GetModuleHandleW(None).unwrap_or_default();
        let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), hmod, 0);
        let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0);
        HOOK_THREAD.store(windows::Win32::System::Threading::GetCurrentThreadId(), Relaxed);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok(h) = kh { let _ = UnhookWindowsHookEx(h); }
        if let Ok(h) = mh { let _ = UnhookWindowsHookEx(h); }
        for s in HELD.iter().chain(SWALLOWED.iter()) {
            s.store(0, Relaxed);
        }
        HOOK_THREAD.store(0, Relaxed);
    });
}

pub fn stop_hook() {
    let tid = HOOK_THREAD.load(Relaxed);
    if tid != 0 && tid != u32::MAX {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

/// Safe mode: register keyboard hotkeys on the core window. Mouse buttons are ignored.
pub fn register_hotkeys(core: HWND, toggle: &Binding, shoot: &Binding) {
    unsafe {
        for id in 1..=2 {
            let _ = UnregisterHotKey(core, id);
        }
        for (id, b) in [(1, toggle), (2, shoot)] {
            if !b.is_bound() || b.vk <= 6 {
                continue;
            }
            let mut m = MOD_NOREPEAT;
            if b.ctrl { m |= MOD_CONTROL; }
            if b.shift { m |= MOD_SHIFT; }
            if b.alt { m |= MOD_ALT; }
            if b.win { m |= MOD_WIN; }
            let _ = RegisterHotKey(core, id, m, b.vk);
        }
    }
}

pub fn unregister_hotkeys(core: HWND) {
    unsafe {
        for id in 1..=2 {
            let _ = UnregisterHotKey(core, id);
        }
    }
}
