#![windows_subsystem = "windows"]

mod capture;
mod config;
mod core;
mod input;
mod ui;
mod watchdog;

use watchdog::{claim_mutex, CORE_MUTEX, UI_MUTEX};

fn main() {
    std::panic::set_hook(Box::new(|info| config::log(&format!("panic: {info}"))));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);

    if has("--watch") {
        let pid = args.iter().position(|a| a == "--watch").and_then(|i| args.get(i + 1)).and_then(|p| p.parse().ok());
        if let Some(pid) = pid {
            watchdog::run(pid);
        }
        return;
    }

    if has("--ui") {
        // One settings window at a time: a second launch just raises the existing one.
        if claim_mutex(UI_MUTEX).is_none() {
            core::open_ui();
            return;
        }
        ui::run();
        return;
    }

    // Core. If one is already running, a plain launch just opens the settings window.
    if claim_mutex(CORE_MUTEX).is_none() {
        if !has("--background") && !has("--restarted") {
            core::open_ui();
        }
        return;
    }
    core::run(has("--restarted"), has("--background"));
}
