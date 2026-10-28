//! Asking the user whether a new website may print, like Zebra Browser Print does.
//!
//! The first request from an unknown origin starts a small "Allow this site to
//! print?" window (through the agent's prompter) and waits for the answer, so
//! the page's first print succeeds once the user clicks Allow. Requests that
//! arrive while the window is open wait for the same answer.
//!
//! The window saves its answer to the config and asks the agent to reload; the
//! reload wakes the waiting requests. Closing the window without answering
//! means "not now": the site is not asked about again until the agent restarts,
//! and it stays in the "waiting" list in Settings.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::{timeout_at, Instant};

use crate::AppState;

/// How long a request waits for the user to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(90);

/// Starts the prompt window for an origin; returns the running child process.
pub type Prompter = Box<dyn Fn(&str) -> std::io::Result<tokio::process::Child> + Send + Sync>;

/// True when `origin` may print, asking the user first if needed.
pub async fn ask(st: &Arc<AppState>, origin: &str) -> bool {
    {
        let printers = st.printers();
        if !printers.ask_new_sites || printers.denied_origins.iter().any(|o| o == origin) {
            return false;
        }
    }
    if st.prompter.is_none() || st.dismissed.lock().unwrap().contains(origin) {
        return false;
    }

    // Subscribe before starting the prompt, so no reload can slip past us.
    let mut changes = st.subscribe();
    if !start_prompt(st, origin) {
        return false;
    }
    let deadline = Instant::now() + ANSWER_WAIT;
    loop {
        if st.origin_allowed(origin) {
            return true;
        }
        if !st.prompting.lock().unwrap().contains(origin) {
            return st.origin_allowed(origin);
        }
        match timeout_at(deadline, changes.changed()).await {
            Ok(Ok(())) => continue,
            _ => return st.origin_allowed(origin),
        }
    }
}

/// Open the prompt unless one is already open for this origin. False if it can't start.
fn start_prompt(st: &Arc<AppState>, origin: &str) -> bool {
    if !st.prompting.lock().unwrap().insert(origin.to_owned()) {
        return true;
    }
    let child = match (st.prompter.as_ref().expect("checked by caller"))(origin) {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(error = %e, "could not open the allow-website window");
            st.prompting.lock().unwrap().remove(origin);
            return false;
        }
    };
    tracing::info!(%origin, "asking whether this website may print");

    let (st, origin) = (st.clone(), origin.to_owned());
    tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
        let decided = {
            let printers = st.printers();
            printers.allowed_origins.contains(&origin) || printers.denied_origins.contains(&origin)
        };
        if !decided {
            st.dismissed.lock().unwrap().insert(origin.clone());
        }
        st.prompting.lock().unwrap().remove(&origin);
        st.notify();
    });
    true
}
