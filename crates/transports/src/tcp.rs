//! Network printers: raw TCP, normally port 9100.

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{timeout, Instant};
use zprint_core::{Error, PrinterInfo, Result, Transport};

use crate::{IDLE_GAP, WRITE_TIMEOUT};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TcpTransport {
    info: PrinterInfo,
    addr: String,
    /// One job at a time per printer, so two labels never interleave.
    lock: Mutex<()>,
}

impl TcpTransport {
    pub fn new(info: PrinterInfo, host: &str, port: u16) -> Self {
        Self {
            info,
            addr: format!("{host}:{port}"),
            lock: Mutex::new(()),
        }
    }

    async fn connect(&self) -> Result<TcpStream> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| Error::Failed(format!("could not reach printer at {}: timed out", self.addr)))?
            .map_err(|e| Error::Failed(format!("could not reach printer at {}: {e}", self.addr)))?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}

#[async_trait]
impl Transport for TcpTransport {
    fn info(&self) -> &PrinterInfo {
        &self.info
    }

    async fn write(&self, data: &[u8]) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut stream = self.connect().await?;
        timeout(WRITE_TIMEOUT, async {
            stream.write_all(data).await?;
            stream.shutdown().await
        })
        .await
        .map_err(|_| Error::Timeout)??;
        Ok(())
    }

    async fn read(&self, query: &[u8], limit: Duration) -> Result<Vec<u8>> {
        let _guard = self.lock.lock().await;
        let mut stream = self.connect().await?;
        stream.write_all(query).await?;

        let deadline = Instant::now() + limit;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = if out.is_empty() {
                deadline - now
            } else {
                IDLE_GAP.min(deadline - now)
            };
            match timeout(wait, stream.read(&mut buf)).await {
                Err(_) | Ok(Ok(0)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Ok(Err(e)) => return Err(e.into()),
            }
        }

        if out.is_empty() {
            return Err(Error::Timeout);
        }
        Ok(out)
    }
}
