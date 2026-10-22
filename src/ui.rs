//! Picker + settings window (egui). Runs as its own process (`--ui`) so a UI crash never
//! affects the hotkeys, tray or watchdog in the core. It edits config.json and pings the core.

use crate::capture::{self, WinInfo};
use crate::config::{Binding, ButtonStyle, Config, Target, Theme};
use crate::core::WM_RELOAD;
use crate::input::WM_SHOOT;
use eframe::egui::{self, Color32, RichText, TextureHandle};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver};
use windows::core::w;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

enum Msg {
    List(Vec<WinInfo>),
    Thumb(isize, [usize; 2], Vec<u8>),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Windows,
    Settings,
}

#[derive(PartialEq, Clone, Copy)]
enum Slot {
    Toggle,
    Shoot,
}

struct App {
    cfg: Config,
    tab: Tab,
    wins: Vec<WinInfo>,
    thumbs: HashMap<isize, TextureHandle>,
    rx: Receiver<Msg>,
    filter: String,
    listening: Option<Slot>,
    applied_theme: Option<Theme>,
}

pub fn run() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([560.0, 400.0]),
        ..Default::default()
    };
    let _ = eframe::run_native("SnapPick", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))));
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = channel();
        let ctx = cc.egui_ctx.clone();
        // Background capture worker: one thumbnail at a time so the UI never stalls.
        std::thread::spawn(move || loop {
            let wins = capture::list_windows();
            if tx.send(Msg::List(wins.clone())).is_err() {
                return;
            }
            ctx.request_repaint();
            for w in wins.iter().filter(|w| !w.minimized) {
                if let Ok(img) = capture::capture(w.hwnd) {
                    let tw = 320u32;
                    let th = ((img.height() as f32 / img.width() as f32) * tw as f32).max(1.0) as u32;
                    let small = image::imageops::thumbnail(&img, tw, th);
                    let size = [small.width() as usize, small.height() as usize];
                    if tx.send(Msg::Thumb(w.hwnd, size, small.into_raw())).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            std::thread::sleep(std::time::Duration::from_millis(600));
        });

        // If the core isn't running (it crashed with no watchdog), bring it back.
        if find_core().is_none() {
            crate::watchdog::spawn_self(&["--background"]);
        }
        App {
            cfg: Config::load(),
            tab: Tab::Windows,
            wins: Vec::new(),
            thumbs: HashMap::new(),
            rx,
            filter: String::new(),
            listening: None,
            applied_theme: None,
        }
    }

    fn commit(&mut self) {
        self.cfg.save();
        notify_core(WM_RELOAD);
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme == Some(self.cfg.theme) {
            return;
        }
        let dark = match self.cfg.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => system_is_dark(),
        };
        ctx.set_visuals(if dark { egui::Visuals::dark() } else { egui::Visuals::light() });
        self.applied_theme = Some(self.cfg.theme);
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::List(w) => {
                    self.thumbs.retain(|h, _| w.iter().any(|x| x.hwnd == *h));
                    self.wins = w;
                }
                Msg::Thumb(h, size, px) => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &px);
                    match self.thumbs.get_mut(&h) {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => {
                            let t = ctx.load_texture(format!("w{h}"), img, egui::TextureOptions::LINEAR);
                            self.thumbs.insert(h, t);
                        }
                    }
                }
            }
        }
    }

    fn select(&mut self, w: &WinInfo) {
        self.cfg.target = Some(Target { hwnd: w.hwnd, title: w.title.clone(), exe: w.exe.clone() });
        self.commit();
    }

    fn windows_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Search");
            ui.text_edit_singleline(&mut self.filter);
        });
        ui.add_space(6.0);
        let filter = self.filter.to_lowercase();
        let wins: Vec<WinInfo> = self
            .wins
            .iter()
            .filter(|w| filter.is_empty() || w.title.to_lowercase().contains(&filter) || w.exe.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        let selected = self.cfg.target.as_ref().map(|t| t.hwnd);
        let mut picked: Option<WinInfo> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            let card_w = 230.0;
            let cols = ((ui.available_width() / (card_w + 12.0)).floor() as usize).max(1);
            egui::Grid::new("grid").spacing([12.0, 12.0]).show(ui, |ui| {
                for (i, w) in wins.iter().enumerate() {
                    let is_sel = selected == Some(w.hwnd);
                    let stroke = if is_sel { egui::Stroke::new(3.0_f32, Color32::from_rgb(88, 101, 242)) } else { egui::Stroke::new(1.0_f32, Color32::from_gray(90)) };
                    let resp = egui::Frame::none()
                        .stroke(stroke)
                        .rounding(8.0)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.set_width(card_w);
                            match self.thumbs.get(&w.hwnd) {
                                Some(t) => {
                                    let aspect = t.size()[1] as f32 / t.size()[0] as f32;
                                    let h = (card_w * aspect).min(150.0);
                                    ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(card_w, h)));
                                }
                                None => {
                                    let (r, _) = ui.allocate_exact_size(egui::vec2(card_w, 120.0), egui::Sense::hover());
                                    ui.painter().rect_filled(r, 4.0, Color32::from_gray(40));
                                    let msg = if w.minimized { "minimized" } else { "loading..." };
                                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::default(), Color32::GRAY);
                                }
                            }
                            ui.add(egui::Label::new(RichText::new(&w.title).strong()).truncate());
                            ui.add(egui::Label::new(RichText::new(&w.exe).weak().small()).truncate());
                        })
                        .response
                        .interact(egui::Sense::click());
                    if resp.clicked() {
                        picked = Some(w.clone());
                    }
                    if (i + 1) % cols == 0 {
                        ui.end_row();
                    }
                }
            });
        });
        if let Some(w) = picked {
            self.select(&w);
        }
    }

    fn binding_row(&mut self, ui: &mut egui::Ui, label: &str, slot: Slot) {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(format!("{label}:"));
            let b = if slot == Slot::Toggle { &mut self.cfg.toggle_bind } else { &mut self.cfg.shoot_bind };
            let listening = self.listening == Some(slot);
            let text = if listening { "Press any key or mouse button... (Esc cancels)".to_string() } else { b.label() };
            if ui.button(text).clicked() {
                self.listening = if listening { None } else { Some(slot) };
            }
            if ui.button("Clear").clicked() {
                *b = Binding::key(0);
                changed = true;
            }
            changed |= ui.checkbox(&mut b.swallow, "Block key from other apps").changed();
        });
        if changed {
            self.commit();
        }
    }

    /// Poll the keyboard/mouse state while recording a binding. Polling (not a hook) means this
    /// also works in safe mode and can see every key and mouse button.
    fn poll_listen(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.listening else { return };
        ctx.request_repaint();
        let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0;
        if down(0x1B) {
            self.listening = None;
            return;
        }
        let (ctrl, shift, alt, win) = (down(0x11), down(0x10), down(0x12), down(0x5B) || down(0x5C));
        let has_mod = ctrl || shift || alt || win;
        for vk in 1..=254i32 {
            // Modifiers can't be the primary key; Left/Right click only count with a modifier
            // (the Listen click itself is a left click).
            if matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5 | 0x03 | 0x07 | 0x0A | 0x0B | 0xE5) {
                continue;
            }
            if (vk == 1 || vk == 2) && !has_mod {
                continue;
            }
            if down(vk) {
                let b = Binding { vk: vk as u32, ctrl, shift, alt, win, swallow: false };
                let target = if slot == Slot::Toggle { &mut self.cfg.toggle_bind } else { &mut self.cfg.shoot_bind };
                let keep_swallow = target.swallow;
                *target = Binding { swallow: keep_swallow, ..b };
                self.listening = None;
                self.commit();
                return;
            }
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut changed = false;

            ui.heading("Hotkeys");
            self.binding_row(ui, "Show / hide screenshot button", Slot::Toggle);
            self.binding_row(ui, "Take screenshot directly", Slot::Shoot);
            if self.cfg.safe_mode {
                ui.colored_label(Color32::from_rgb(230, 160, 40), "Safe mode: keyboard keys only, and keys cannot be blocked from other apps.");
            }
            ui.add_space(10.0);

            ui.heading("Screenshot button");
            changed |= ui.checkbox(&mut self.cfg.floating_button, "Floating button on screen (drag to move, right-click to hide)").changed();
            changed |= ui.checkbox(&mut self.cfg.picker_button, "Button inside this window").changed();
            ui.horizontal(|ui| {
                ui.label("Button style:");
                for (s, n) in [(ButtonStyle::Dark, "Dark"), (ButtonStyle::Light, "White")] {
                    changed |= ui.selectable_value(&mut self.cfg.button_style, s, n).changed();
                }
            });
            ui.add_space(10.0);

            ui.heading("Feedback");
            changed |= ui.checkbox(&mut self.cfg.sound, "Sound").changed();
            changed |= ui.checkbox(&mut self.cfg.flash, "Flash over the window").changed();
            changed |= ui.checkbox(&mut self.cfg.toast, "Corner notification (errors always show)").changed();
            ui.add_space(10.0);

            ui.heading("Saving");
            changed |= ui.checkbox(&mut self.cfg.save_to_folder, "Also save a PNG to a folder").changed();
            ui.horizontal(|ui| {
                ui.label("Folder:");
                changed |= ui.text_edit_singleline(&mut self.cfg.save_folder).lost_focus();
                if ui.button("Browse...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&self.cfg.save_folder).pick_folder() {
                        self.cfg.save_folder = dir.to_string_lossy().into_owned();
                        changed = true;
                    }
                }
                if ui.button("Open").clicked() {
                    let _ = std::fs::create_dir_all(&self.cfg.save_folder);
                    let _ = std::process::Command::new("explorer").arg(&self.cfg.save_folder).spawn();
                }
            });
            ui.add_space(10.0);

            ui.heading("Reliability");
            changed |= ui.checkbox(&mut self.cfg.watchdog, "Watchdog: restart SnapPick if it crashes or is killed").changed();
            ui.label(RichText::new("Quit from the tray icon always closes everything for real.").weak());
            changed |= ui
                .checkbox(&mut self.cfg.safe_mode, "Anti-cheat safe mode: no global input hook")
                .on_hover_text("Uses standard Windows hotkey registration instead. Keyboard only. Lowers, but cannot remove, the chance that anti-cheat objects.")
                .changed();
            changed |= ui.checkbox(&mut self.cfg.autostart, "Start with Windows").changed();
            ui.add_space(10.0);

            ui.heading("Appearance");
            ui.horizontal(|ui| {
                for (t, n) in [(Theme::System, "System"), (Theme::Dark, "Dark"), (Theme::Light, "Light")] {
                    changed |= ui.selectable_value(&mut self.cfg.theme, t, n).changed();
                }
            });

            if changed {
                self.commit();
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_theme(ctx);
        self.drain(ctx);
        self.poll_listen(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Windows, "Windows");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.cfg.picker_button && ui.button("Screenshot").clicked() {
                        notify_core(WM_SHOOT);
                    }
                    let sel = match &self.cfg.target {
                        Some(t) => format!("Selected: {}", t.title),
                        None => "No window selected".into(),
                    };
                    ui.add(egui::Label::new(RichText::new(sel).weak()).truncate());
                });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Windows => self.windows_tab(ui),
            Tab::Settings => self.settings_tab(ui),
        });
    }
}

fn find_core() -> Option<windows::Win32::Foundation::HWND> {
    unsafe { FindWindowW(w!("SnapPickCoreWnd"), None).ok().filter(|h| !h.is_invalid()) }
}

fn notify_core(msg: u32) {
    if let Some(h) = find_core() {
        unsafe {
            let _ = PostMessageW(h, msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn system_is_dark() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(true)
}
