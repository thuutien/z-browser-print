//! CUPS queues, printed with `lp -o raw` so ZPL reaches the printer unchanged.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;
use zprint_core::{Error, PrinterInfo, Result, Transport};

use crate::WRITE_TIMEOUT;

pub struct CupsTransport {
    info: PrinterInfo,
    queue: String,
    lock: Mutex<()>,
}

impl CupsTransport {
    pub fn new(info: PrinterInfo, queue: String) -> Self {
        Self {
            info,
            queue,
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl Transport for CupsTransport {
    fn info(&self) -> &PrinterInfo {
        &self.info
    }

    async fn write(&self, data: &[u8]) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut child = Command::new("lp")
            .args(["-d", &self.queue, "-o", "raw"])
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    Error::Failed("the 'lp' command is not installed (package cups-client)".into())
                }
                _ => Error::Failed(format!("could not run lp: {e}")),
            })?;

        let mut stdin = child.stdin.take().expect("stdin is piped");
        let data = data.to_vec();
        let job = async move {
            stdin.write_all(&data).await?;
            drop(stdin);
            child.wait_with_output().await
        };
        let output = timeout(WRITE_TIMEOUT, job).await.map_err(|_| Error::Timeout)??;
        if !output.status.success() {
            let msg = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(Error::Failed(if msg.is_empty() { "lp failed".into() } else { msg }));
        }
        Ok(())
    }

    async fn read(&self, _query: &[u8], _limit: Duration) -> Result<Vec<u8>> {
        Err(Error::Unsupported(
            "CUPS printers can't answer status queries; connect over network or USB for that",
        ))
    }
}
