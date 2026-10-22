# SnapPick

A small Windows tool for taking a screenshot of **one app window** and putting it on the clipboard. Pick the window once from a Discord-style grid, then use a hotkey, a mouse button or a floating camera button to grab it.

## Features

- **Window picker** with live thumbnails, like Discord's stream picker.
- **Captures only the selected window**, even if other windows cover it. The result goes to the clipboard and can also be saved as a PNG.
- **Floating camera button** (dark or white style) that you show or hide with a hotkey. Drag it to move it, right-click to hide it.
- **Two hotkeys**: one toggles the button, one takes the screenshot directly. You can bind any keyboard key or mouse button, and choose whether the key is blocked from reaching other apps.
- **Feedback** you can switch off individually: sound, flash and a corner toast.
- **Survivability**: the hotkeys and tray live in their own process, separate from the settings window, and an optional watchdog restarts SnapPick if it crashes. Quitting from the tray icon always closes everything.
- **Anti-cheat safe mode**: skips the global input hook and uses standard Windows hotkey registration (keyboard only). This lowers the chance of conflicts with anti-cheat software but cannot remove it.
- Start with Windows, light/dark/system theme.

## Install

Download `SnapPick-v1.0-windows-x64.exe` from [Releases](../../releases) and run it. Windows 10/11, 64-bit. The exe is not code-signed, so SmartScreen may warn.

Default hotkeys: **F9** show/hide the button, **F10** take a screenshot. Change them in Settings.

## Build from source

Needs [Rust](https://rustup.rs) and the Visual Studio C++ Build Tools.

```
cargo build --release
```

The exe is `target/release/snappick.exe`.

## Privacy

SnapPick has no networking code. Settings live in `%APPDATA%\SnapPick\config.json` and a small `log.txt` next to it. Screenshots go only to your clipboard, plus the folder you choose if saving is on.

## Known limits

- Minimized windows can't be captured; you get a "minimized" notice.
- Games in exclusive fullscreen may capture black, and the floating button can't draw over them.
- Some anti-cheat systems block any third-party hotkey or overlay tool.
- The floating button forgets its dragged position after a restart.
