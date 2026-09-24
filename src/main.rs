use std::process::ExitCode;
use std::sync::Arc;

use git_serve::{api, config::Config, mirror::Mirror};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let mirror = Arc::new(tokio::task::spawn_blocking(move || Mirror::open(config.mirror)).await??);
    // The first sync starts now and overlaps with binding; requests wait for it.
    let worker = mirror.spawn();

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(addr = %config.bind, "listening");
    let signalled = Arc::clone(&mirror);
    axum::serve(listener, api::router(Arc::clone(&mirror)))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            // Interrupts a running sync so requests waiting for it finish.
            signalled.shutdown();
        })
        .await?;
    mirror.shutdown();
    worker.await?;
    Ok(())
}

/// Ctrl-C, or SIGTERM from the platform stopping the sandbox.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
