use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};

/// A hotkey: a primary key or mouse button (as a virtual-key code) plus exact modifiers.
/// `vk == 0` means unbound.
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Debug)]
pub struct Binding {
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    /// Block the key from reaching the focused app (only possible with the low-level hook).
    pub swallow: bool,
}

impl Binding {
    pub const fn key(vk: u32) -> Self {
        Binding { vk, ctrl: false, shift: false, alt: false, win: false, swallow: false }
    }

    pub fn is_bound(&self) -> bool {
        self.vk != 0
    }

    /// Packed form read by the hook callback without locking.
    pub fn pack(&self) -> u64 {
        if !self.is_bound() {
            return 0;
        }
        (self.vk as u64 & 0xFF)
            | (self.ctrl as u64) << 8
            | (self.shift as u64) << 9
            | (self.alt as u64) << 10
            | (self.win as u64) << 11
            | (self.swallow as u64) << 12
            | 1 << 13
    }

    pub fn label(&self) -> String {
        if !self.is_bound() {
            return "Not set".into();
        }
        let mut s = String::new();
        if self.ctrl { s += "Ctrl + "; }
        if self.shift { s += "Shift + "; }
        if self.alt { s += "Alt + "; }
        if self.win { s += "Win + "; }
        s + &vk_name(self.vk)
    }
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x01 => "Left Mouse".into(),
        0x02 => "Right Mouse".into(),
        0x04 => "Middle Mouse".into(),
        0x05 => "Mouse Back (X1)".into(),
        0x06 => "Mouse Forward (X2)".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x13 => "Pause".into(),
        0x14 => "Caps Lock".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "Print Screen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x60..=0x69 => format!("Numpad {}", vk - 0x60),
        0x6A => "Numpad *".into(),
        0x6B => "Numpad +".into(),
        0x6D => "Numpad -".into(),
        0x6E => "Numpad .".into(),
        0x6F => "Numpad /".into(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x90 => "Num Lock".into(),
        0x91 => "Scroll Lock".into(),
        _ => {
            let sc = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
            let mut buf = [0u16; 64];
            let n = unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::GetKeyNameTextW((sc << 16) as i32, &mut buf)
            };
            if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                format!("Key 0x{vk:02X}")
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Target {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Debug)]
pub enum Theme {
    System,
    Dark,
    Light,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub toggle_bind: Binding,
    pub shoot_bind: Binding,
    pub floating_button: bool,
    pub picker_button: bool,
    pub watchdog: bool,
    /// Skip the low-level input hook; use plain RegisterHotKey instead (keyboard only).
    pub safe_mode: bool,
    pub autostart: bool,
    pub theme: Theme,
    pub sound: bool,
    pub toast: bool,
    pub flash: bool,
    pub save_to_folder: bool,
    pub save_folder: String,
    pub target: Option<Target>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            toggle_bind: Binding::key(0x78), // F9
            shoot_bind: Binding::key(0x79),  // F10
            floating_button: true,
            picker_button: true,
            watchdog: true,
            safe_mode: false,
            autostart: false,
            theme: Theme::System,
            sound: true,
            toast: true,
            flash: true,
            save_to_folder: false,
            save_folder: default_pictures_dir().to_string_lossy().into_owned(),
            target: None,
        }
    }
}

fn default_pictures_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Pictures").join("SnapPick")
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(base).join("SnapPick");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    /// Write via a temp file so a crash mid-save never leaves a corrupt config.
    pub fn save(&self) {
        let path = config_path();
        let tmp = path.with_extension("json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(self) {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir().join("log.txt")) {
        let _ = writeln!(f, "{msg}");
    }
}
