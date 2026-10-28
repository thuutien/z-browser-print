//! Tray icon (StatusNotifierItem over D-Bus): pick the default printer, open
//! Settings, open the test page, quit. Clicking the icon opens Settings.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use ksni::menu::{RadioGroup, RadioItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip, TrayMethods};
use zprint_core::config::Config;
use zprint_server::AppState;
use zprint_settings::APP_NAME;

/// 64x64 RGBA app icon, rendered from data/zprint.svg.
const ICON_RGBA: &[u8] = include_bytes!("../../../data/zprint-64.rgba");

pub struct ZTray {
    state: Arc<AppState>,
    config_path: PathBuf,
}

/// Show the tray icon and keep its menu in step with config reloads.
pub async fn spawn(state: Arc<AppState>, config_path: PathBuf) -> anyhow::Result<()> {
    let mut changes = state.subscribe();
    let handle = ZTray { state, config_path }.spawn().await?;
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            handle.update(|_| {}).await;
        }
    });
    Ok(())
}

impl ZTray {
    fn open_settings(&self) {
        spawn_detached(
            Command::new(zprint_core::program_path().unwrap_or_else(|_| "zprint".into()))
                .arg("settings")
                .arg("--config")
                .arg(&self.config_path),
        );
    }

    fn set_default(&self, id: String) {
        let result = Config::load(&self.config_path).and_then(|mut config| {
            config.default_printer = Some(id.clone());
            config.save(&self.config_path)
        });
        match result.and_then(|()| self.state.reload()) {
            Ok(()) => tracing::info!(printer = %id, "default printer changed from tray"),
            Err(e) => tracing::warn!(error = %e, "could not change default printer"),
        }
    }

    fn default_name(&self) -> String {
        self.state
            .printers()
            .default()
            .map(|p| p.info().name.clone())
            .unwrap_or_else(|| "none".into())
    }
}

fn spawn_detached(cmd: &mut Command) {
    let cmd = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!(error = %e, "could not start {:?}", cmd.get_program()),
    }
}

impl ksni::Tray for ZTray {
    fn id(&self) -> String {
        "zprint".into()
    }

    fn title(&self) -> String {
        APP_NAME.into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        // StatusNotifierItem wants ARGB32 in network byte order.
        let data = ICON_RGBA
            .chunks_exact(4)
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect();
        vec![Icon { width: 64, height: 64, data }]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: APP_NAME.into(),
            description: format!("Default printer: {}", self.default_name()),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_settings();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let printers = self.state.printers();
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: format!("{APP_NAME} – listening on 127.0.0.1:{}", self.state.port),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];

        if printers.list.is_empty() {
            items.push(
                StandardItem {
                    label: "No printers – open Settings to add one".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            items.push(
                StandardItem {
                    label: "Default printer:".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            let ids: Vec<String> = printers.list.iter().map(|p| p.info().id.clone()).collect();
            let default_id = printers.default().map(|p| p.info().id.clone());
            items.push(
                RadioGroup {
                    selected: ids.iter().position(|id| Some(id) == default_id.as_ref()).unwrap_or(0),
                    select: Box::new(move |this: &mut Self, i| {
                        if let Some(id) = ids.get(i) {
                            this.set_default(id.clone());
                        }
                    }),
                    options: printers
                        .list
                        .iter()
                        .map(|p| RadioItem {
                            label: format!("{}  ({})", p.info().name, p.info().address),
                            ..Default::default()
                        })
                        .collect(),
                }
                .into(),
            );
        }

        let port = self.state.port;
        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|this: &mut Self| this.open_settings()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open test page".into(),
                activate: Box::new(move |_: &mut Self| {
                    spawn_detached(Command::new("xdg-open").arg(format!("http://127.0.0.1:{port}")))
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}
