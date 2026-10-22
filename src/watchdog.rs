//! Process plumbing: single-instance mutexes, spawning helper processes, and the watchdog.
//!
//! The watchdog is a tiny hidden `snappick.exe --watch <pid>` process. It waits for the core
//! to exit and relaunches it unless the exit code was 0 (tray "Quit", logoff) or the stop
//! event was signalled (watchdog disabled in settings). Core and watchdog also watch each
//! other: the core respawns a missing watchdog from its housekeeping timer.

use crate::config::{log, Config};
use std::collections::VecDeque;
use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;

pub const CORE_MUTEX: &str = "Local\\SnapPickCore";
pub const UI_MUTEX: &str = "Local\\SnapPickUi";
pub const WATCHDOG_MUTEX: &str = "Local\\SnapPickWatchdog";
const STOP_EVENT: &str = "Local\\SnapPickWatchdogStop";

/// Create a named mutex. `Some` = we are the first (and now only) owner; `None` = already held.
/// The handle is intentionally leaked: the mutex lives as long as the process.
pub fn claim_mutex(name: &str) -> Option<HANDLE> {
    unsafe {
        let h = CreateMutexW(None, false, &HSTRING::from(name)).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(h);
            return None;
        }
        Some(h)
    }
}

pub fn mutex_exists(name: &str) -> bool {
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(name)) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Launch this exe again with `args`, detached from our console/job where possible.
pub fn spawn_self(args: &[&str]) -> Option<u32> {
    let exe = std::env::current_exe().ok()?;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const BREAKAWAY: u32 = 0x0100_0000;
    for flags in [CREATE_NO_WINDOW | DETACHED_PROCESS | BREAKAWAY, CREATE_NO_WINDOW | DETACHED_PROCESS] {
        if let Ok(c) = std::process::Command::new(&exe).args(args).creation_flags(flags).spawn() {
            return Some(c.id());
        }
    }
    None
}

pub fn stop_watchdog() {
    unsafe {
        if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(STOP_EVENT)) {
            let _ = SetEvent(ev);
            let _ = CloseHandle(ev);
        }
    }
}

/// Make the running state match `cfg.watchdog`.
pub fn sync(cfg: &Config) {
    let running = mutex_exists(WATCHDOG_MUTEX);
    if cfg.watchdog && !running {
        spawn_self(&["--watch", &std::process::id().to_string()]);
    } else if !cfg.watchdog && running {
        stop_watchdog();
    }
}

pub fn run(mut pid: u32) {
    let Some(_guard) = claim_mutex(WATCHDOG_MUTEX) else { return };
    unsafe {
        let Ok(stop) = CreateEventW(None, true, false, &HSTRING::from(STOP_EVENT)) else { return };
        let _ = ResetEvent(stop);
        let mut restarts: VecDeque<Instant> = VecDeque::new();

        loop {
            let code = match OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(ph) => {
                    let r = WaitForMultipleObjects(&[ph, stop], false, INFINITE);
                    if r.0 != WAIT_OBJECT_0.0 {
                        let _ = CloseHandle(ph);
                        return; // stop event (or wait failure): do not restart anything
                    }
                    let mut code = 1u32;
                    let _ = GetExitCodeProcess(ph, &mut code);
                    let _ = CloseHandle(ph);
                    code
                }
                // Core is already gone before we could attach: treat as a crash.
                Err(_) => 1,
            };
            if code == 0 || WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 || !Config::load().watchdog {
                return;
            }
            let now = Instant::now();
            restarts.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
            if restarts.len() >= 5 {
                log("watchdog: core crashed 5 times in a minute, giving up");
                return;
            }
            restarts.push_back(now);
            log(&format!("watchdog: core exited with code {code:#x}, restarting"));
            std::thread::sleep(Duration::from_millis(500));
            match spawn_self(&["--restarted"]) {
                Some(p) => pid = p,
                None => return,
            }
        }
    }
}
