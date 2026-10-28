//! Talking to the running agent from the settings window: a tiny blocking HTTP
//! client for localhost (no Origin header, so admin calls are allowed).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

fn request(port: u16, method: &str, path: &str) -> std::io::Result<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let code = response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    Ok((code, body))
}

/// The agent's version when it is running on `port`.
pub fn running_version(port: u16) -> Option<String> {
    let (code, body) = request(port, "GET", "/v1/version").ok()?;
    if code != 200 {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    json["version"].as_str().map(str::to_owned)
}

/// Websites that tried to print but are not allowed yet.
pub fn blocked(port: u16) -> Vec<String> {
    match request(port, "GET", "/v1/admin/blocked") {
        Ok((200, body)) => serde_json::from_str(&body).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Ask the agent to re-read the config file.
pub fn reload(port: u16) -> Result<(), String> {
    match request(port, "POST", "/v1/admin/reload") {
        Ok((204, _)) => Ok(()),
        Ok((_, body)) => Err(serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|j| j["error"].as_str().map(str::to_owned))
            .unwrap_or(body)),
        Err(e) => Err(format!("the agent is not running ({e})")),
    }
}

/// Start the agent in the background (this same program without "settings").
pub fn start(config_path: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group, so closing a terminal doesn't take the agent down with it.
        .process_group(0)
        .spawn()?;
    // Reap it if it exits while the window is open, so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}
