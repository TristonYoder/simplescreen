use screen_server::{bonjour, build_state, config::Config, router, BRAND_SLUG};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = Config::from_env()?;
    let state = build_state(cfg.clone()).await?;
    let listener = tokio::net::TcpListener::bind((cfg.bind.as_str(), cfg.port)).await?;
    let port = listener.local_addr()?.port();
    tracing::info!("{BRAND_SLUG} server {} listening on {}:{port}", env!("CARGO_PKG_VERSION"), cfg.bind);
    tracing::info!("data dir {}", cfg.data_dir.display());

    let daemon = if cfg.bonjour { bonjour::advertise(port, &state.server_id) } else { None };

    let shutdown_tx = state.shutdown.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            let _ = shutdown_tx.send(true);
        })
        .await?;

    if let Some(d) = daemon {
        let _ = d.shutdown();
    }
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
