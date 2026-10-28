//! USB printers through the kernel usblp driver (/dev/usb/lp*).
//!
//! Device files are plain blocking I/O, so all work runs on tokio's blocking pool.
//! Reads use O_NONBLOCK and a polling loop, because a usblp read can otherwise
//! wait forever when the printer has nothing to say.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::timeout;
use zprint_core::{Error, PrinterInfo, Result, Transport};

use crate::{discover, IDLE_GAP, WRITE_TIMEOUT};

const POLL: Duration = Duration::from_millis(20);

pub struct UsbTransport {
    info: PrinterInfo,
    device: String,
    serial: Option<String>,
    lock: Mutex<()>,
}

impl UsbTransport {
    pub fn new(info: PrinterInfo, device: String, serial: Option<String>) -> Self {
        Self {
            info,
            device,
            serial,
            lock: Mutex::new(()),
        }
    }

    /// The printer's current device file: found by serial number when we know it
    /// (lpN changes when printers are replugged), else the configured path.
    fn path(&self) -> String {
        self.serial
            .as_deref()
            .and_then(|serial| {
                discover::usb_devices()
                    .into_iter()
                    .find(|d| d.serial.as_deref() == Some(serial))
            })
            .map(|d| d.path)
            .unwrap_or_else(|| self.device.clone())
    }
}

fn open_error(path: &str, e: std::io::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => Error::Failed(format!(
            "{path} does not exist. Is the printer plugged in and switched on?"
        )),
        ErrorKind::PermissionDenied => Error::Failed(format!(
            "no permission to use {path}. Add yourself to the 'lp' group \
             (sudo usermod -aG lp $USER) and log in again"
        )),
        _ => Error::Failed(format!("could not open {path}: {e}")),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Failed(format!("USB worker failed: {e}")))?
}

#[async_trait]
impl Transport for UsbTransport {
    fn info(&self) -> &PrinterInfo {
        &self.info
    }

    async fn write(&self, data: &[u8]) -> Result<()> {
        let _guard = self.lock.lock().await;
        let path = self.path();
        let data = data.to_vec();
        let job = blocking(move || {
            let mut file = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|e| open_error(&path, e))?;
            file.write_all(&data)?;
            file.flush()?;
            Ok(())
        });
        timeout(WRITE_TIMEOUT, job).await.map_err(|_| Error::Timeout)?
    }

    async fn read(&self, query: &[u8], limit: Duration) -> Result<Vec<u8>> {
        let _guard = self.lock.lock().await;
        let path = self.path();
        let query = query.to_vec();
        blocking(move || {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .map_err(|e| open_error(&path, e))?;
            let deadline = Instant::now() + limit;
            write_until(&mut file, &query, deadline)?;
            read_reply(&mut file, deadline)
        })
        .await
    }
}

fn write_until(file: &mut File, mut data: &[u8], deadline: Instant) -> Result<()> {
    while !data.is_empty() {
        match file.write(data) {
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(Error::Timeout);
                }
                std::thread::sleep(POLL);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn read_reply(file: &mut File, deadline: Instant) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut last_data = Instant::now();
    loop {
        let now = Instant::now();
        if now >= deadline || (!out.is_empty() && now - last_data >= IDLE_GAP) {
            break;
        }
        match file.read(&mut buf) {
            Ok(n) if n > 0 => {
                out.extend_from_slice(&buf[..n]);
                last_data = Instant::now();
            }
            // usblp returns 0 or EAGAIN while the printer has nothing to send.
            Ok(_) => std::thread::sleep(POLL),
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(e) => return Err(e.into()),
        }
    }
    if out.is_empty() {
        return Err(Error::Timeout);
    }
    Ok(out)
}
