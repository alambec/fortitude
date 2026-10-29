mod admin;
mod app;
mod archive;
mod auth;
mod config;
mod metrics;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::serve::ListenerExt;
use clap::Parser;
use fortitude_store::{Store, StoreOptions};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, LogFormat};

#[derive(Debug, Parser)]
#[command(version, about = "High performance DICOMweb server")]
struct Cli {
    /// TOML configuration file.
    #[arg(short, long, env = "FORTITUDE_CONFIG")]
    config: Option<PathBuf>,
}

/// Largest object of a single S3 PutObject request.
const S3_MAX_OBJECT_MIB: usize = 5 * 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("invalid configuration")?;
    init_tracing(&config);

    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let index = config
        .index
        .backend()
        .map_err(anyhow::Error::msg)
        .context("invalid index configuration")?;
    let files = config
        .storage
        .files()
        .map_err(anyhow::Error::msg)
        .context("invalid storage configuration")?;
    if matches!(files, fortitude_store::files::FileBackend::S3(_))
        && config.dicomweb.max_instance_size_mib > S3_MAX_OBJECT_MIB
    {
        anyhow::bail!(
            "with the s3 storage backend, dicomweb.max_instance_size_mib cannot exceed \
             {S3_MAX_OBJECT_MIB} (the PutObject limit)"
        );
    }
    let store = Store::open(StoreOptions {
        path: config.storage.path.clone(),
        files,
        fsync: config.storage.fsync,
        index,
        index_readers: match config.index.readers {
            0 => cpus,
            n => n,
        },
        inline_binary_max: config.storage.inline_binary_max,
    })
    .with_context(|| format!("cannot open storage at {}", config.storage.path.display()))?;
    store
        .files()
        .check()
        .await
        .with_context(|| format!("cannot reach the file storage {:?}", store.files()))?;
    tracing::info!(files = ?store.files(), "storage opened");
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
    if config.http.ui && config.auth.users.is_empty() {
        tracing::warn!("no user configured: the web explorer is disabled");
    }
    let dimse = config
        .dimse
        .service(
            config
                .dicomweb
                .max_instance_size_mib
                .saturating_mul(1 << 20),
        )
        .map_err(anyhow::Error::msg)
        .context("invalid DIMSE configuration")?;
    let app = app::build(&config, store.clone(), metrics_handle);
    let shutdown = CancellationToken::new();

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
    let dimse = match config.dimse.listen {
        Some(address) => {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("cannot listen on {address} (DIMSE)"))?;
            tracing::info!(listen = %address, ae_title = dimse.ae_title, "DIMSE service started");
            let dimse = fortitude_dimse::Dimse {
                store,
                config: Arc::new(dimse),
            };
            Some(tokio::spawn(fortitude_dimse::serve(
                listener,
                dimse,
                shutdown.clone().cancelled_owned(),
            )))
        }
        None => None,
    };
    tracing::info!(listen = %config.http.listen, "fortitude started");

    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown_signal().await;
            shutdown.cancel();
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    if let Some(dimse) = dimse {
        dimse.await?;
    }
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
