//! The Z Browser Print settings window: add and remove printers, choose the
//! default, test them, and manage allowed websites and start-at-login.

mod agent;
mod autostart;
mod prompt;

pub use prompt::run as ask_origin;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use zprint_core::config::{Config, Connection, PrinterConfig, RAW_PORT};
use zprint_core::status::{parse_host_status, HostStatus};
use zprint_transports::discover::{self, CupsQueue, UsbDevice};
use zprint_transports::netscan::{self, NetworkPrinter};

pub const APP_NAME: &str = "Z Browser Print";
const AGENT_POLL: Duration = Duration::from_secs(3);
const OK: egui::Color32 = egui::Color32::from_rgb(40, 160, 80);
const WARN: egui::Color32 = egui::Color32::from_rgb(214, 132, 20);
/// 64x64 RGBA app icon, rendered from data/zprint.svg.
pub(crate) const ICON_RGBA: &[u8] = include_bytes!("../../../data/zprint-64.rgba");

pub fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id("zprint")
            .with_inner_size([600.0, 420.0])
            .with_min_inner_size([460.0, 320.0])
            .with_icon(egui::IconData { rgba: ICON_RGBA.to_vec(), width: 64, height: 64 }),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            cc.egui_ctx.all_styles_mut(compact_style);
            Ok(Box::new(SettingsApp::new(config_path)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not open the settings window: {e}"))
}

pub(crate) fn compact_style(style: &mut egui::Style) {
    use egui::{FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(16.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
        (TextStyle::Small, FontId::proportional(11.0)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(7.0, 2.0);
    style.spacing.interact_size.y = 20.0;
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Printers,
    Websites,
    General,
}

#[derive(Clone)]
enum Scan {
    Idle,
    Running,
    Done(Vec<NetworkPrinter>),
}

#[derive(Clone)]
struct Note {
    text: String,
    error: bool,
}

#[derive(Clone, PartialEq)]
enum AgentStatus {
    Unknown,
    Running(String),
    Stopped,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Network,
    Usb,
    Cups,
}

struct AddForm {
    kind: Kind,
    name: String,
    host: String,
    port: String,
    usb: usize,
    cups: usize,
}

enum Action {
    Save(&'static str),
    SetDefault(String),
    Remove(String),
    TestPrint(String),
    Status(String),
    Add,
    AddOrigin,
    AllowOrigin(String),
    RemoveOrigin(usize),
    Unblock(String),
    SetAskNewSites(bool),
    SetZebraCompat(bool),
    SetAutostart(bool),
    StartAgent,
    RefreshDevices,
    ScanNetwork,
}

struct SettingsApp {
    path: PathBuf,
    config: Config,
    rt: tokio::runtime::Runtime,
    /// Last message for the status bar; written by background jobs too.
    note: Arc<Mutex<Option<Note>>>,
    agent: Arc<Mutex<AgentStatus>>,
    /// Websites the agent refused, so the user can allow them with one click.
    blocked: Arc<Mutex<Vec<String>>>,
    agent_checked: Option<Instant>,
    form: AddForm,
    usb: Vec<UsbDevice>,
    cups: Vec<CupsQueue>,
    new_origin: String,
    autostart: bool,
    rename_pending: bool,
    tab: Tab,
    /// The add-printer form is open.
    adding: bool,
    /// "Find printers" results, filled in by a background scan.
    scan: Arc<Mutex<Scan>>,
}

impl SettingsApp {
    fn new(path: PathBuf) -> Self {
        let (config, first_note) = match Config::load(&path) {
            Ok(c) => (c, None),
            Err(e) => (
                Config::default(),
                Some(Note {
                    text: format!("Could not read {}: {e:#}. Saving will replace it.", path.display()),
                    error: true,
                }),
            ),
        };
        let mut app = Self {
            path,
            config,
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime"),
            note: Arc::new(Mutex::new(first_note)),
            agent: Arc::new(Mutex::new(AgentStatus::Unknown)),
            blocked: Arc::new(Mutex::new(Vec::new())),
            agent_checked: None,
            form: AddForm {
                kind: Kind::Network,
                name: String::new(),
                host: String::new(),
                port: RAW_PORT.to_string(),
                usb: 0,
                cups: 0,
            },
            usb: Vec::new(),
            cups: Vec::new(),
            new_origin: String::new(),
            autostart: autostart::is_enabled(),
            rename_pending: false,
            tab: Tab::Printers,
            adding: false,
            scan: Arc::new(Mutex::new(Scan::Idle)),
        };
        app.refresh_devices();

        // Like Zebra Browser Print: opening the settings makes sure the agent runs.
        if agent::running_version(app.config.port).is_none() {
            app.start_agent();
        }
        app
    }

    fn set_note(&self, text: impl Into<String>, error: bool) {
        *self.note.lock().unwrap() = Some(Note { text: text.into(), error });
    }

    fn refresh_devices(&mut self) {
        self.usb = discover::usb_devices();
        self.cups = discover::cups_queues();
        self.form.usb = 0;
        self.form.cups = 0;
    }

    fn start_agent(&mut self) {
        match agent::start(&self.path) {
            Ok(()) => self.set_note(format!("Started {APP_NAME}."), false),
            Err(e) => self.set_note(format!("Could not start the agent: {e}"), true),
        }
        self.agent_checked = None;
    }

    /// Check in the background whether the agent is running, every few seconds.
    fn poll_agent(&mut self, ctx: &egui::Context) {
        if self.agent_checked.is_some_and(|t| t.elapsed() < AGENT_POLL) {
            return;
        }
        self.agent_checked = Some(Instant::now());
        let (port, status, blocked, repaint) =
            (self.config.port, self.agent.clone(), self.blocked.clone(), ctx.clone());
        std::thread::spawn(move || {
            let now = match agent::running_version(port) {
                Some(v) => AgentStatus::Running(v),
                None => AgentStatus::Stopped,
            };
            *blocked.lock().unwrap() = match now {
                AgentStatus::Running(_) => agent::blocked(port),
                _ => Vec::new(),
            };
            *status.lock().unwrap() = now;
            repaint.request_repaint();
        });
        ctx.request_repaint_after(AGENT_POLL);
    }

    /// Save the config and tell the agent to pick it up.
    fn save(&self, what: &str) {
        if let Err(e) = self.config.save(&self.path) {
            self.set_note(format!("Could not save: {e:#}"), true);
            return;
        }
        let (port, note, what) = (self.config.port, self.note.clone(), what.to_owned());
        std::thread::spawn(move || {
            let text = match agent::reload(port) {
                Ok(()) => Note { text: what, error: false },
                Err(e) => Note { text: format!("{what} The agent did not reload: {e}"), error: true },
            };
            *note.lock().unwrap() = Some(text);
        });
    }

    fn printer(&self, id: &str) -> Option<PrinterConfig> {
        self.config.printers.iter().find(|p| p.id == id).cloned()
    }

    fn test_print(&self, id: &str, ctx: &egui::Context) {
        let Some(printer) = self.printer(id) else { return };
        let zpl = format!(
            "^XA^CI28\
             ^FO30,30^A0N,40,40^FDZ Browser Print test^FS\
             ^FO30,85^A0N,28,28^FD{}^FS\
             ^FO30,125^A0N,24,24^FDIf you can read this, printing works.^FS\
             ^XZ",
            printer.display_name().replace(['^', '~'], " ")
        );
        self.set_note(format!("Printing a test label on {}…", printer.display_name()), false);
        let (note, ctx) = (self.note.clone(), ctx.clone());
        self.rt.spawn(async move {
            let name = printer.display_name().to_owned();
            let result = zprint_transports::open(&printer).write(zpl.as_bytes()).await;
            *note.lock().unwrap() = Some(match result {
                Ok(()) => Note { text: format!("Sent a test label to {name}."), error: false },
                Err(e) => Note { text: format!("{name}: {e}"), error: true },
            });
            ctx.request_repaint();
        });
    }

    fn check_status(&self, id: &str, ctx: &egui::Context) {
        let Some(printer) = self.printer(id) else { return };
        self.set_note(format!("Asking {} for its status…", printer.display_name()), false);
        let (note, ctx) = (self.note.clone(), ctx.clone());
        self.rt.spawn(async move {
            let name = printer.display_name().to_owned();
            let result = zprint_transports::open(&printer)
                .read(b"~HS", Duration::from_secs(3))
                .await
                .and_then(|raw| parse_host_status(&raw));
            *note.lock().unwrap() = Some(match result {
                Ok(s) => Note { text: format!("{name}: {}", describe(&s)), error: !s.ready },
                Err(e) => Note { text: format!("{name}: {e}"), error: true },
            });
            ctx.request_repaint();
        });
    }

    fn add_printer(&mut self) {
        let f = &self.form;
        let (connection, fallback_name) = match f.kind {
            Kind::Network => {
                let host = f.host.trim();
                if host.is_empty() {
                    return self.set_note("Enter the printer's IP address or host name.", true);
                }
                let Ok(port) = f.port.trim().parse::<u16>() else {
                    return self.set_note("The port must be a number (usually 9100).", true);
                };
                (Connection::Network { host: host.to_owned(), port }, host.to_owned())
            }
            Kind::Usb => {
                let Some(d) = self.usb.get(f.usb) else {
                    return self.set_note("No USB printer is selected.", true);
                };
                let name = format!("{} {}", d.manufacturer, d.product).trim().to_owned();
                (
                    Connection::Usb { device: d.path.clone(), serial: d.serial.clone() },
                    if name.is_empty() { d.path.clone() } else { name },
                )
            }
            Kind::Cups => {
                let Some(q) = self.cups.get(f.cups) else {
                    return self.set_note("No CUPS printer is selected.", true);
                };
                (Connection::Cups { queue: q.name.clone() }, q.name.replace('_', " "))
            }
        };
        let name = match f.name.trim() {
            "" => fallback_name,
            n => n.to_owned(),
        };
        let id = self.config.new_id(&name);
        if self.config.printers.is_empty() {
            self.config.default_printer = Some(id.clone());
        }
        self.config.printers.push(PrinterConfig { id, name: name.clone(), connection });
        self.form.name.clear();
        self.form.host.clear();
        self.adding = false;
        self.save(&format!("Added {name}."));
    }

    fn add_origin(&mut self) {
        match normalize_origin(&self.new_origin) {
            Ok(origin) => {
                if !self.config.allowed_origins.contains(&origin) {
                    self.config.allowed_origins.push(origin.clone());
                    self.save(&format!("{origin} can now print."));
                }
                self.new_origin.clear();
            }
            Err(msg) => self.set_note(msg, true),
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Save(what) => self.save(what),
            Action::SetDefault(id) => {
                let name = self.printer(&id).map(|p| p.display_name().to_owned()).unwrap_or_default();
                self.config.default_printer = Some(id);
                self.save(&format!("{name} is now the default printer."));
            }
            Action::Remove(id) => {
                let name = self.printer(&id).map(|p| p.display_name().to_owned()).unwrap_or_default();
                self.config.remove_printer(&id);
                self.save(&format!("Removed {name}."));
            }
            Action::TestPrint(id) => self.test_print(&id, ctx),
            Action::Status(id) => self.check_status(&id, ctx),
            Action::Add => self.add_printer(),
            Action::AddOrigin => self.add_origin(),
            Action::AllowOrigin(origin) => {
                self.blocked.lock().unwrap().retain(|o| *o != origin);
                self.config.denied_origins.retain(|o| *o != origin);
                if !self.config.allowed_origins.contains(&origin) {
                    self.config.allowed_origins.push(origin.clone());
                    self.save(&format!("{origin} can now print."));
                }
            }
            Action::Unblock(origin) => {
                self.config.denied_origins.retain(|o| *o != origin);
                self.save(&format!("Unblocked {origin}. It will be asked about next time."));
            }
            Action::SetAskNewSites(on) => {
                self.config.ask_new_sites = on;
                self.save(if on { "New websites will be asked about." } else { "New websites will be refused." });
            }
            Action::SetZebraCompat(on) => {
                self.config.zebra_compat = on;
                self.save("Saved. Quit from the tray and start the app again to apply this.");
            }
            Action::RemoveOrigin(i) => {
                if i < self.config.allowed_origins.len() {
                    let origin = self.config.allowed_origins.remove(i);
                    self.save(&format!("{origin} can no longer print."));
                }
            }
            Action::SetAutostart(on) => match autostart::set(on, &self.path) {
                Ok(()) => {
                    self.autostart = on;
                    let when = if on { "will start" } else { "will not start" };
                    self.set_note(format!("{APP_NAME} {when} when you log in."), false);
                }
                Err(e) => {
                    self.autostart = autostart::is_enabled();
                    self.set_note(format!("Could not change start-at-login: {e:#}"), true);
                }
            },
            Action::StartAgent => self.start_agent(),
            Action::ScanNetwork => {
                *self.scan.lock().unwrap() = Scan::Running;
                let (scan, ctx) = (self.scan.clone(), ctx.clone());
                self.rt.spawn(async move {
                    let found = netscan::scan().await;
                    *scan.lock().unwrap() = Scan::Done(found);
                    ctx.request_repaint();
                });
            }
            Action::RefreshDevices => {
                self.refresh_devices();
                self.set_note(
                    format!("Found {} USB printer(s) and {} CUPS printer(s).", self.usb.len(), self.cups.len()),
                    false,
                );
            }
        }
    }

    // ---- drawing ------------------------------------------------------------

    fn header(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading(APP_NAME);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.agent.lock().unwrap().clone() {
                    AgentStatus::Running(v) => {
                        ui.weak(format!("v{v}"));
                        ui.colored_label(OK, "● Running");
                    }
                    AgentStatus::Stopped => {
                        if ui.small_button("Start").clicked() {
                            actions.push(Action::StartAgent);
                        }
                        ui.colored_label(ui.visuals().error_fg_color, "● Stopped");
                    }
                    AgentStatus::Unknown => {
                        ui.weak("● Checking…");
                    }
                }
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let waiting = self.blocked.lock().unwrap().len();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Printers, format!("Printers ({})", self.config.printers.len()));
            let sites = if waiting > 0 {
                egui::RichText::new(format!("Websites • {waiting} waiting")).color(WARN)
            } else {
                egui::RichText::new(format!("Websites ({})", self.config.allowed_origins.len()))
            };
            ui.selectable_value(&mut self.tab, Tab::Websites, sites);
            ui.selectable_value(&mut self.tab, Tab::General, "General");
        });
    }

    fn printers_tab(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.config.printers.is_empty() {
            self.adding = true;
        } else {
            let default_id = self.config.default_printer_id().map(str::to_owned);
            egui::Grid::new("printers")
                .num_columns(3)
                .striped(true)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    for p in &mut self.config.printers {
                        let is_default = default_id.as_deref() == Some(p.id.as_str());
                        if ui.radio(is_default, "").on_hover_text("Default printer").clicked() && !is_default {
                            actions.push(Action::SetDefault(p.id.clone()));
                        }
                        ui.vertical(|ui| {
                            ui.set_min_width(250.0);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let name = ui
                                .add(egui::TextEdit::singleline(&mut p.name).frame(egui::Frame::NONE).desired_width(250.0))
                                .on_hover_text("Click to rename");
                            // Saved once the field loses focus, not on every keystroke.
                            if name.changed() {
                                self.rename_pending = true;
                            }
                            if name.lost_focus() && self.rename_pending {
                                self.rename_pending = false;
                                actions.push(Action::Save("Renamed the printer."));
                            }
                            let detail = format!("{} · {}", kind_label(&p.connection), p.connection.address());
                            ui.add(egui::Label::new(egui::RichText::new(detail).small().weak()).truncate());
                        });
                        ui.horizontal(|ui| {
                            if ui.small_button("Test").on_hover_text("Print a test label").clicked() {
                                actions.push(Action::TestPrint(p.id.clone()));
                            }
                            let can_ask = !matches!(p.connection, Connection::Cups { .. });
                            if ui
                                .add_enabled(can_ask, egui::Button::new("Status").small())
                                .on_hover_text("Ask the printer if it is ready")
                                .on_disabled_hover_text("CUPS printers can't report their status")
                                .clicked()
                            {
                                actions.push(Action::Status(p.id.clone()));
                            }
                            if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                actions.push(Action::Remove(p.id.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
            ui.add_space(6.0);
        }

        if self.adding {
            self.add_form(ui, actions);
        } else if ui.button("+ Add printer").clicked() {
            self.adding = true;
        }
    }

    fn add_form(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let can_cancel = !self.config.printers.is_empty();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let f = &mut self.form;
            ui.horizontal(|ui| {
                ui.strong("Add printer");
                ui.add_space(8.0);
                ui.selectable_value(&mut f.kind, Kind::Network, "Network");
                ui.selectable_value(&mut f.kind, Kind::Usb, "USB");
                ui.selectable_value(&mut f.kind, Kind::Cups, "CUPS");
            });
            ui.add_space(2.0);
            egui::Grid::new("add").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                match f.kind {
                    Kind::Network => {
                        ui.label("Address");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("192.168.1.50").desired_width(160.0));
                            ui.label("Port");
                            ui.add(egui::TextEdit::singleline(&mut f.port).desired_width(48.0));
                            if matches!(*self.scan.lock().unwrap(), Scan::Running) {
                                ui.spinner();
                                ui.weak("Looking…");
                            } else if ui
                                .small_button("Find printers")
                                .on_hover_text("Look for printers on your network (port 9100)")
                                .clicked()
                            {
                                actions.push(Action::ScanNetwork);
                            }
                        });
                        ui.end_row();
                        let found = match &*self.scan.lock().unwrap() {
                            Scan::Done(found) => Some(found.clone()),
                            _ => None,
                        };
                        if let Some(found) = found {
                            ui.label("");
                            ui.vertical(|ui| {
                                if found.is_empty() {
                                    ui.weak("Nothing on your network answered on port 9100.");
                                }
                                for p in &found {
                                    let host = p.host.to_string();
                                    let added = self.config.printers.iter().any(|c| {
                                        matches!(&c.connection, Connection::Network { host: h, .. } if *h == host)
                                    });
                                    ui.horizontal(|ui| {
                                        if ui.add_enabled(!added, egui::Button::new("Use").small()).clicked() {
                                            f.host = host.clone();
                                            f.port = "9100".into();
                                            if let Some(d) = &p.description {
                                                f.name = d.clone();
                                            }
                                        }
                                        let label = egui::RichText::new(p.label());
                                        ui.label(if p.is_zebra { label.strong() } else { label });
                                        if added {
                                            ui.weak("(added)");
                                        }
                                    });
                                }
                            });
                            ui.end_row();
                        }
                    }
                    Kind::Usb => {
                        ui.label("Printer");
                        ui.horizontal(|ui| {
                            if self.usb.is_empty() {
                                ui.weak("None found. Plug it in and switch it on.");
                            } else {
                                egui::ComboBox::from_id_salt("usb")
                                    .width(260.0)
                                    .selected_text(self.usb[f.usb.min(self.usb.len() - 1)].label())
                                    .show_ui(ui, |ui| {
                                        for (i, d) in self.usb.iter().enumerate() {
                                            ui.selectable_value(&mut f.usb, i, d.label());
                                        }
                                    });
                            }
                            if ui.small_button("⟳").on_hover_text("Look again").clicked() {
                                actions.push(Action::RefreshDevices);
                            }
                        });
                        ui.end_row();
                    }
                    Kind::Cups => {
                        ui.label("Printer");
                        ui.horizontal(|ui| {
                            if self.cups.is_empty() {
                                ui.weak("CUPS has no printers.");
                            } else {
                                let label = |q: &CupsQueue| {
                                    if q.is_zebra { format!("{} (Zebra)", q.name) } else { q.name.clone() }
                                };
                                egui::ComboBox::from_id_salt("cups")
                                    .width(260.0)
                                    .selected_text(label(&self.cups[f.cups.min(self.cups.len() - 1)]))
                                    .show_ui(ui, |ui| {
                                        for (i, q) in self.cups.iter().enumerate() {
                                            ui.selectable_value(&mut f.cups, i, label(q));
                                        }
                                    });
                            }
                            if ui.small_button("⟳").on_hover_text("Look again").clicked() {
                                actions.push(Action::RefreshDevices);
                            }
                        });
                        ui.end_row();
                    }
                }
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("optional").desired_width(260.0));
                ui.end_row();
            });
            let hint = match f.kind {
                Kind::Network => "Zebra printers listen on port 9100.",
                Kind::Usb => "Uses /dev/usb/lp*. You need to be in the 'lp' group.",
                Kind::Cups => "Sent raw (lp -o raw). CUPS printers can't report status.",
            };
            ui.label(egui::RichText::new(hint).small().weak());
            ui.horizontal(|ui| {
                if ui.button("Add").clicked() {
                    actions.push(Action::Add);
                }
                if can_cancel && ui.button("Cancel").clicked() {
                    self.adding = false;
                }
            });
        });
    }

    fn websites_tab(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let waiting = self.blocked.lock().unwrap().clone();
        if !waiting.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("Waiting for approval").strong().color(WARN));
                ui.label(egui::RichText::new("These websites tried to print.").small().weak());
                for origin in waiting {
                    ui.horizontal(|ui| {
                        if ui.small_button("Allow").clicked() {
                            actions.push(Action::AllowOrigin(origin.clone()));
                        }
                        ui.monospace(&origin);
                    });
                }
            });
            ui.add_space(6.0);
        }

        ui.label("Websites allowed to print");
        if self.config.allowed_origins.is_empty() {
            ui.label(
                egui::RichText::new("None yet. A website shows up above after it tries to print.").small().weak(),
            );
        }
        for (i, origin) in self.config.allowed_origins.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("✕").on_hover_text("Remove").clicked() {
                    actions.push(Action::RemoveOrigin(i));
                }
                ui.monospace(origin);
            });
        }
        if !self.config.denied_origins.is_empty() {
            ui.add_space(6.0);
            ui.label("Blocked websites");
            for origin in &self.config.denied_origins {
                ui.horizontal(|ui| {
                    if ui.small_button("Unblock").on_hover_text("Ask again next time it tries to print").clicked() {
                        actions.push(Action::Unblock(origin.clone()));
                    }
                    ui.monospace(origin);
                });
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.new_origin)
                    .hint_text("https://app.example.com")
                    .desired_width(240.0),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Add").clicked() || enter {
                actions.push(Action::AddOrigin);
            }
        });
    }

    fn general_tab(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let mut on = self.autostart;
        if ui.checkbox(&mut on, "Start at login").changed() {
            actions.push(Action::SetAutostart(on));
        }
        let mut ask = self.config.ask_new_sites;
        if ui
            .checkbox(&mut ask, "Ask when a new website wants to print")
            .on_hover_text("Otherwise new websites are refused until you allow them in the Websites tab")
            .changed()
        {
            actions.push(Action::SetAskNewSites(ask));
        }
        let mut zebra = self.config.zebra_compat;
        if ui
            .checkbox(&mut zebra, "Answer apps made for Zebra Browser Print (port 9100)")
            .on_hover_text("Lets web apps that use Zebra's BrowserPrint JS library print without changes")
            .changed()
        {
            actions.push(Action::SetZebraCompat(zebra));
        }
        ui.add_space(8.0);
        let url = format!("http://127.0.0.1:{}", self.config.port);
        egui::Grid::new("info").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.weak("Web API");
            ui.horizontal(|ui| {
                ui.monospace(&url);
                ui.hyperlink_to("test page", &url);
            });
            ui.end_row();
            ui.weak("Settings");
            ui.add(egui::Label::new(egui::RichText::new(self.path.display().to_string()).monospace().small()).truncate());
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!(
                "Closing this window keeps {APP_NAME} running in the tray. Quit it from the tray menu."
            ))
            .small()
            .weak(),
        );
    }

    fn note_line(&self, ui: &mut egui::Ui) {
        match self.note.lock().unwrap().clone() {
            Some(note) => {
                let color = if note.error { ui.visuals().error_fg_color } else { OK };
                ui.add(egui::Label::new(egui::RichText::new(note.text).small().color(color)).truncate());
            }
            None => {
                ui.label(egui::RichText::new(" ").small());
            }
        }
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_agent(&ctx);
        let mut actions = Vec::new();

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            self.header(ui, &mut actions);
            ui.add_space(4.0);
            self.tab_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("note").show(ui, |ui| {
            ui.add_space(3.0);
            self.note_line(ui);
            ui.add_space(3.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.tab {
                Tab::Printers => self.printers_tab(ui, &mut actions),
                Tab::Websites => self.websites_tab(ui, &mut actions),
                Tab::General => self.general_tab(ui, &mut actions),
            });
        });

        for action in actions {
            self.apply(action, &ctx);
        }
    }
}

fn kind_label(c: &Connection) -> &'static str {
    match c {
        Connection::Network { .. } => "Network",
        Connection::Usb { .. } => "USB",
        Connection::Cups { .. } => "CUPS",
    }
}

fn describe(s: &HostStatus) -> String {
    let problems: Vec<&str> = [
        (s.paper_out, "out of labels"),
        (s.head_open, "print head open"),
        (s.ribbon_out && s.thermal_transfer, "out of ribbon"),
        (s.paused, "paused"),
        (s.buffer_full, "buffer full"),
    ]
    .into_iter()
    .filter_map(|(on, text)| on.then_some(text))
    .collect();
    if problems.is_empty() {
        "Ready".into()
    } else {
        problems.join(", ")
    }
}

/// "https://App.Example.com/" -> "https://app.example.com"; rejects paths and other schemes.
fn normalize_origin(input: &str) -> Result<String, String> {
    let s = input.trim().trim_end_matches('/');
    let (scheme, rest) = s
        .split_once("://")
        .ok_or("Enter a full address starting with https:// or http://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err("The address must start with https:// or http://".into());
    }
    if rest.is_empty() || rest.contains(['/', '?', '#', ' ']) {
        return Err("Enter only the site address, without a page path (e.g. https://app.example.com)".into());
    }
    Ok(format!("{scheme}://{}", rest.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::normalize_origin;

    #[test]
    fn normalizes_origins() {
        assert_eq!(normalize_origin(" https://App.Example.com/ ").unwrap(), "https://app.example.com");
        assert_eq!(normalize_origin("http://10.0.0.2:8080").unwrap(), "http://10.0.0.2:8080");
        assert!(normalize_origin("app.example.com").is_err());
        assert!(normalize_origin("https://app.example.com/print").is_err());
        assert!(normalize_origin("ftp://x").is_err());
    }
}
