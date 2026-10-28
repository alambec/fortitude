mod admin;
mod app;
mod auth;
mod config;
mod metrics;

use std::path::PathBuf;

use anyhow::Context;
use axum::serve::ListenerExt;
use clap::Parser;
use fortitude_store::{Store, StoreOptions};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, LogFormat};

#[derive(Debug, Parser)]
#[command(version, about = "High performance DICOMweb server")]
struct Cli {
    /// TOML configuration file.
    #[arg(short, long, env = "FORTITUDE_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("invalid configuration")?;
    init_tracing(&config);

    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let store = Store::open(StoreOptions {
        path: config.storage.path.clone(),
        fsync: config.storage.fsync,
        index_readers: match config.index.readers {
            0 => cpus,
            n => n,
        },
        inline_binary_max: config.storage.inline_binary_max,
    })
    .with_context(|| format!("cannot open storage at {}", config.storage.path.display()))?;
    let metrics_handle = if config.metrics.enabled {
        let recorder = metrics::recorder();
        let handle = recorder.handle();
        ::metrics::set_global_recorder(recorder).context("cannot install the metrics recorder")?;
        Some(handle)
    } else {
        None
    };
    if config.auth.users.is_empty() && config.auth.tokens.is_empty() {
        tracing::warn!("no credentials configured: authentication is disabled");
    }
    let app = app::build(&config, store, metrics_handle);

    // Without TCP_NODELAY, the last segment of a streamed response can wait for the
    // client's delayed ACK (~40 ms).
    let listener = tokio::net::TcpListener::bind(config.http.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.http.listen))?
        .tap_io(|tcp| {
            if let Err(e) = tcp.set_nodelay(true) {
                tracing::warn!(error = %e, "cannot set TCP_NODELAY");
            }
        });
    tracing::info!(listen = %config.http.listen, "fortitude started");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("fortitude stopped");
    Ok(())
}

fn init_tracing(config: &Config) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log.filter));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log.format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
