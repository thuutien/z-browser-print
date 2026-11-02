//! One settings window per user. The first window listens on a Unix socket; a
//! second `zprint settings` (another tray click, the app menu) pokes it so it comes
//! to the front, then exits.

use std::io::ErrorKind;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

pub enum Claim {
    /// We are the only window. Connections on the listener mean "come to the front".
    Owner(UnixListener, PathBuf),
    /// Another window is open and has been asked to come to the front.
    AlreadyOpen,
    /// No socket could be made; run anyway rather than not open at all.
    Unavailable,
}

fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("zprint-settings.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_default();
            std::env::temp_dir().join(format!("zprint-settings-{user}.sock"))
        }
    }
}

pub fn claim() -> Claim {
    let path = socket_path();
    for _ in 0..2 {
        match UnixListener::bind(&path) {
            Ok(listener) => return Claim::Owner(listener, path),
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    return Claim::AlreadyOpen;
                }
                // Left behind by a window that crashed; nobody is listening.
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => break,
        }
    }
    Claim::Unavailable
}
