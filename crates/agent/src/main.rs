//! Z Browser Print (command: zprint): lets web pages print raw ZPL to local Zebra printers.
//!
//!   zprint [--config <path>]            run the agent (HTTP API + tray icon)
//!   zprint settings [--config <path>]   open the settings window

mod tray;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use tracing_subscriber::EnvFilter;
use zprint_core::config::{Config, ZEBRA_PORT};
use zprint_server::{AppState, Printers};

enum Mode {
    Agent,
    Settings,
}

fn parse_args() -> anyhow::Result<(Mode, Option<PathBuf>)> {
    let mut mode = Mode::Agent;
    let mut config = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "settings" => mode = Mode::Settings,
            "--config" => config = Some(args.next().map(PathBuf::from).context("--config needs a path")?),
            "-h" | "--help" => {
                println!("usage: zprint [settings] [--config <path>]");
                std::process::exit(0);
            }
            _ => bail!("unknown argument: {arg} (usage: zprint [settings] [--config <path>])"),
        }
    }
    Ok((mode, config))
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let (mode, config) = parse_args()?;
    let path = match config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    match mode {
        // The window must own the main thread, so it runs outside tokio.
        Mode::Settings => zprint_settings::run(path),
        Mode::Agent => tokio::runtime::Runtime::new()?.block_on(run_agent(path)),
    }
}

async fn run_agent(path: PathBuf) -> anyhow::Result<()> {
    if !path.exists() {
        Config::default().save(&path)?;
        tracing::info!(path = %path.display(), "created config");
    }
    let initial = Config::load(&path)?;
    let port = initial.port;

    let config_path = path.clone();
    let loader = Box::new(move || {
        let config = Config::load(&config_path)?;
        Ok(Printers {
            list: config.printers.iter().map(zprint_transports::open).collect(),
            default_printer: config.default_printer,
            allowed_origins: config.allowed_origins,
        })
    });
    let zebra_port = initial.zebra_compat.then_some(ZEBRA_PORT);
    let state = Arc::new(AppState::new(port, zebra_port, loader)?);
    tracing::info!(path = %path.display(), printers = state.printers().list.len(), "loaded config");

    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::info!("something is already listening on 127.0.0.1:{port}; Z Browser Print is probably running");
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("cannot listen on 127.0.0.1:{port}")),
    };
    tracing::info!("Z Browser Print listening on http://127.0.0.1:{port}");

    // Zebra's BrowserPrint JS library looks for Browser Print on 127.0.0.1:9100.
    if let Some(zport) = zebra_port {
        match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, zport)).await {
            Ok(listener) => {
                tracing::info!("answering Zebra BrowserPrint on http://127.0.0.1:{zport}");
                let app = zprint_server::router(state.clone());
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::warn!(error = %e, "BrowserPrint listener stopped");
                    }
                });
            }
            Err(e) => tracing::warn!(
                error = %e,
                "cannot listen on 127.0.0.1:{zport}, so web apps using Zebra's BrowserPrint \
                 library won't find Z Browser Print (is Zebra Browser Print or something else using it?)"
            ),
        }
    }

    if let Err(e) = tray::spawn(state.clone(), path).await {
        tracing::warn!(error = %e, "no tray icon (is a StatusNotifier host running?)");
    }

    axum::serve(listener, zprint_server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
