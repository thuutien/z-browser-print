//! Shared types for zprint: printer description, errors and the `Transport` trait
//! that every connection type (network, USB, spooler, serial) implements.

pub mod config;
pub mod status;

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("printer not found: {0}")]
    NotFound(String),
    #[error("timed out talking to printer")]
    Timeout,
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad response from printer: {0}")]
    BadResponse(String),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Failed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize)]
pub struct PrinterInfo {
    pub id: String,
    pub name: String,
    /// "network", "usb" or "cups".
    pub connection: &'static str,
    /// Human readable address, e.g. "192.168.1.50:9100", "/dev/usb/lp0", a CUPS queue.
    pub address: String,
}

#[async_trait]
pub trait Transport: Send + Sync {
    fn info(&self) -> &PrinterInfo;

    /// Send raw bytes (ZPL, EPL, ...) to the printer unchanged.
    async fn write(&self, data: &[u8]) -> Result<()>;

    /// Send `query` and collect the printer's reply until it goes quiet or `timeout` passes.
    async fn read(&self, query: &[u8], timeout: Duration) -> Result<Vec<u8>>;
}
