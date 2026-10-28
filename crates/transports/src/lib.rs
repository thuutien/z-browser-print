//! Connection types that implement `zprint_core::Transport`, and printer discovery.

pub mod cups;
pub mod discover;
pub mod tcp;
pub mod usb;

use std::sync::Arc;
use std::time::Duration;

pub use cups::CupsTransport;
pub use tcp::TcpTransport;
pub use usb::UsbTransport;
use zprint_core::config::{Connection, PrinterConfig};
use zprint_core::{PrinterInfo, Transport};

/// Once the printer has started answering, this much silence means the reply is complete.
pub(crate) const IDLE_GAP: Duration = Duration::from_millis(300);
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the transport for a configured printer.
pub fn open(printer: &PrinterConfig) -> Arc<dyn Transport> {
    let info = PrinterInfo {
        id: printer.id.clone(),
        name: printer.display_name().to_owned(),
        connection: printer.connection.kind(),
        address: printer.connection.address(),
    };
    match &printer.connection {
        Connection::Network { host, port } => Arc::new(TcpTransport::new(info, host, *port)),
        Connection::Usb { device, serial } => {
            Arc::new(UsbTransport::new(info, device.clone(), serial.clone()))
        }
        Connection::Cups { queue } => Arc::new(CupsTransport::new(info, queue.clone())),
    }
}
