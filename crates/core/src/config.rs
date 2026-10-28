//! zprint configuration, shared by the agent, the tray and the settings window.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 9180;
pub const RAW_PORT: u16 = 9100;
/// Where Zebra's BrowserPrint JS library looks for Browser Print.
pub const ZEBRA_PORT: u16 = 9100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Local port the agent listens on (always bound to 127.0.0.1).
    #[serde(default = "default_port")]
    pub port: u16,
    /// Also answer Zebra's BrowserPrint JS library on 127.0.0.1:9100.
    #[serde(default = "yes")]
    pub zebra_compat: bool,
    /// Web sites allowed to print, exactly as the browser sends Origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Web sites the user refused to let print; never asked about again.
    #[serde(default)]
    pub denied_origins: Vec<String>,
    /// Ask the user when an unknown website tries to print.
    #[serde(default = "yes")]
    pub ask_new_sites: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_printer: Option<String>,
    #[serde(default)]
    pub printers: Vec<PrinterConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrinterConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub connection: Connection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Connection {
    /// Raw TCP, normally port 9100.
    Network {
        host: String,
        #[serde(default = "raw_port")]
        port: u16,
    },
    /// USB printer through the kernel's usblp driver (/dev/usb/lp*).
    Usb {
        device: String,
        /// USB serial number; lets us find the printer again if it moves to another lpN.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        serial: Option<String>,
    },
    /// CUPS queue, printed with `lp -o raw`. Cannot answer status queries.
    Cups { queue: String },
}

impl Connection {
    pub fn kind(&self) -> &'static str {
        match self {
            Connection::Network { .. } => "network",
            Connection::Usb { .. } => "usb",
            Connection::Cups { .. } => "cups",
        }
    }

    pub fn address(&self) -> String {
        match self {
            Connection::Network { host, port } => format!("{host}:{port}"),
            Connection::Usb { device, .. } => device.clone(),
            Connection::Cups { queue } => queue.clone(),
        }
    }
}

impl PrinterConfig {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            zebra_compat: true,
            allowed_origins: Vec::new(),
            denied_origins: Vec::new(),
            ask_new_sites: true,
            default_printer: None,
            printers: Vec::new(),
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn yes() -> bool {
    true
}

fn raw_port() -> u16 {
    RAW_PORT
}

impl Config {
    /// `$ZPRINT_CONFIG`, else `~/.config/zprint/config.toml`.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        if let Some(p) = std::env::var_os("ZPRINT_CONFIG") {
            return Ok(PathBuf::from(p));
        }
        let dirs = directories::ProjectDirs::from("", "", "zprint")
            .context("cannot find a config directory for this user")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Load and check the config. A missing file gives the default config.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        config.validate()?;
        Ok(config)
    }

    /// Write the config atomically (temp file + rename), so the agent never reads half a file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = HashSet::new();
        for p in &self.printers {
            if p.id.is_empty() {
                bail!("a printer has an empty id");
            }
            if !seen.insert(p.id.as_str()) {
                bail!("printer id {:?} is used more than once", p.id);
            }
        }
        if let Some(id) = &self.default_printer {
            if !seen.contains(id.as_str()) {
                bail!("default_printer {id:?} is not in the printers list");
            }
        }
        Ok(())
    }

    /// The chosen default printer, or the first one when none is chosen.
    pub fn default_printer_id(&self) -> Option<&str> {
        self.default_printer
            .as_deref()
            .or_else(|| self.printers.first().map(|p| p.id.as_str()))
    }

    /// A short id made from `name` that no other printer uses.
    pub fn new_id(&self, name: &str) -> String {
        let mut base: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        base = base.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
        if base.is_empty() {
            base = "printer".into();
        }
        let taken = |id: &str| self.printers.iter().any(|p| p.id == id);
        if !taken(&base) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap()
    }

    /// Remove a printer, keeping `default_printer` valid.
    pub fn remove_printer(&mut self, id: &str) {
        self.printers.retain(|p| p.id != id);
        if self.default_printer.as_deref() == Some(id) {
            self.default_printer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_printer_types() {
        let config = Config {
            port: 9180,
            zebra_compat: false,
            allowed_origins: vec!["https://app.example.com".into()],
            denied_origins: vec!["https://evil.example".into()],
            ask_new_sites: false,
            default_printer: Some("usb".into()),
            printers: vec![
                PrinterConfig {
                    id: "net".into(),
                    name: "Office".into(),
                    connection: Connection::Network { host: "10.0.0.5".into(), port: 9100 },
                },
                PrinterConfig {
                    id: "usb".into(),
                    name: "Desk".into(),
                    connection: Connection::Usb { device: "/dev/usb/lp0".into(), serial: Some("ABC".into()) },
                },
                PrinterConfig {
                    id: "cups".into(),
                    name: String::new(),
                    connection: Connection::Cups { queue: "Zebra_ZD420".into() },
                },
            ],
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn new_id_is_unique_slug() {
        let mut config = Config::default();
        assert_eq!(config.new_id("Zebra ZD420 (office)"), "zebra-zd420-office");
        config.printers.push(PrinterConfig {
            id: "zebra".into(),
            name: String::new(),
            connection: Connection::Cups { queue: "q".into() },
        });
        assert_eq!(config.new_id("Zebra"), "zebra-2");
        assert_eq!(config.new_id("!!!"), "printer");
    }

    #[test]
    fn rejects_unknown_default() {
        let config = Config { default_printer: Some("nope".into()), ..Config::default() };
        assert!(config.validate().is_err());
    }
}
