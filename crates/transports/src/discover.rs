//! Finding printers this computer can see: USB (usblp) devices and CUPS queues.
//! These are quick, blocking calls meant for the settings window.

use std::path::Path;
use std::process::Command;

const ZEBRA_VENDOR_ID: &str = "0a5f";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    /// e.g. "/dev/usb/lp0"
    pub path: String,
    pub manufacturer: String,
    pub product: String,
    pub serial: Option<String>,
    pub is_zebra: bool,
}

impl UsbDevice {
    pub fn label(&self) -> String {
        let name = format!("{} {}", self.manufacturer, self.product);
        let name = name.trim();
        if name.is_empty() {
            self.path.clone()
        } else {
            format!("{name} ({})", self.path)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CupsQueue {
    pub name: String,
    /// Device URI, e.g. "usb://Zebra%20Technologies/ZTC%20ZD420-203dpi?serial=..."
    pub uri: String,
    pub is_zebra: bool,
}

/// USB printers handled by the usblp kernel driver, Zebra first.
pub fn usb_devices() -> Vec<UsbDevice> {
    let mut found: Vec<UsbDevice> = match std::fs::read_dir("/sys/class/usbmisc") {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.starts_with("lp").then(|| usb_device(&name))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort_by(|a, b| b.is_zebra.cmp(&a.is_zebra).then(a.path.cmp(&b.path)));
    found
}

fn usb_device(name: &str) -> UsbDevice {
    // /sys/class/usbmisc/lpN/device is the USB interface; its parent is the device
    // with the vendor, product and serial attributes.
    let usb = std::fs::canonicalize(format!("/sys/class/usbmisc/{name}/device"))
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let attr = |file: &str| {
        usb.as_ref()
            .and_then(|dir| std::fs::read_to_string(dir.join(file)).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    UsbDevice {
        path: format!("/dev/usb/{name}"),
        manufacturer: attr("manufacturer").unwrap_or_default(),
        product: attr("product").unwrap_or_default(),
        serial: attr("serial"),
        is_zebra: attr("idVendor").as_deref() == Some(ZEBRA_VENDOR_ID),
    }
}

/// CUPS queues from `lpstat -v`, Zebra first. Empty when CUPS is not installed.
pub fn cups_queues() -> Vec<CupsQueue> {
    let Ok(output) = Command::new("lpstat").arg("-v").env("LC_ALL", "C").output() else {
        return Vec::new();
    };
    let mut queues = parse_lpstat_v(&String::from_utf8_lossy(&output.stdout));
    queues.sort_by(|a, b| b.is_zebra.cmp(&a.is_zebra).then(a.name.cmp(&b.name)));
    queues
}

fn parse_lpstat_v(text: &str) -> Vec<CupsQueue> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("device for ")?;
            let (name, uri) = rest.split_once(':')?;
            let uri = uri.trim().to_owned();
            let is_zebra = format!("{name} {uri}").to_lowercase().contains("zebra");
            Some(CupsQueue { name: name.to_owned(), uri, is_zebra })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lpstat() {
        let text = "device for EPSON_ET_3750: implicitclass://EPSON_ET_3750/\n\
                    device for ZD420: usb://Zebra%20Technologies/ZTC%20ZD420-203dpi?serial=X\n\
                    garbage line\n";
        let q = parse_lpstat_v(text);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].name, "EPSON_ET_3750");
        assert!(!q[0].is_zebra);
        assert_eq!(q[1].uri, "usb://Zebra%20Technologies/ZTC%20ZD420-203dpi?serial=X");
        assert!(q[1].is_zebra);
    }
}
