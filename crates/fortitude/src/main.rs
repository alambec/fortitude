mod config;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use fortitude_dicomweb::DicomWeb;
use fortitude_store::{Store, StoreOptions};
use tower_http::trace::TraceLayer;
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
    let dicomweb = DicomWeb {
        store,
        config: Arc::new(fortitude_dicomweb::Config {
            root: config.http.dicomweb_root.clone(),
            public_url: config.http.public_url.clone(),
            stow_concurrency: config.dicomweb.stow_concurrency,
            max_instance_size: config
                .dicomweb
                .max_instance_size_mib
                .saturating_mul(1 << 20),
        }),
    };

    let app = Router::new()
        .route("/health", get(health))
        .nest(
            &config.http.dicomweb_root,
            fortitude_dicomweb::router(dicomweb),
        )
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(config.http.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.http.listen))?;
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

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
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
