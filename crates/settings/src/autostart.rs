//! "Start at login" through XDG autostart entries, which Cinnamon, GNOME, KDE,
//! XFCE and MATE all honour.
//!
//! The .deb installs /etc/xdg/autostart/zprint.desktop, so zprint starts for every
//! user like Zebra Browser Print does. A user turns it off with a same-named entry
//! in ~/.config/autostart that says Hidden=true. Without the system entry (running
//! from a build folder), turning it on writes a user entry instead.

use std::path::{Path, PathBuf};

use zprint_core::config::Config;

const SYSTEM_ENTRY: &str = "/etc/xdg/autostart/zprint.desktop";

fn user_entry() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("autostart").join("zprint.desktop"))
}

fn system_entry() -> Option<&'static Path> {
    Some(Path::new(SYSTEM_ENTRY)).filter(|p| p.exists())
}

pub fn is_enabled() -> bool {
    match user_entry().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(text) => !text.lines().any(|l| l.trim() == "Hidden=true"),
        None => system_entry().is_some(),
    }
}

pub fn set(enabled: bool, config_path: &Path) -> anyhow::Result<()> {
    let path = user_entry().ok_or_else(|| anyhow::anyhow!("cannot find your config directory"))?;
    let entry = match (enabled, system_entry().is_some()) {
        // The system entry already starts zprint; drop any user override.
        (true, true) => None,
        (true, false) => Some(desktop_entry(config_path, false)?),
        (false, true) => Some(desktop_entry(config_path, true)?),
        (false, false) => None,
    };
    match entry {
        Some(text) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, text)?;
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        },
    }
    Ok(())
}

fn desktop_entry(config_path: &Path, hidden: bool) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    // Only name the config file when it isn't the usual one.
    let config_arg = match Config::default_path() {
        Ok(default) if default == config_path => String::new(),
        _ => format!(" --config \"{}\"", config_path.display()),
    };
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Z Browser Print\n\
         Comment=Let web pages print to Zebra label printers\n\
         Exec=\"{}\"{config_arg}\n\
         Icon=zprint\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         {}",
        exe.display(),
        if hidden { "Hidden=true\n" } else { "" }
    ))
}
