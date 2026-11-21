use std::os::unix::fs::{FileTypeExt, PermissionsExt};

use screen_server::{bonjour, build_state, config::Config, router, BRAND_SLUG};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = Config::from_env()?;
    let state = build_state(cfg.clone()).await?;
    tracing::info!("data dir {}", cfg.data_dir.display());
    tokio::spawn(screen_server::schedule::run(state.clone()));

    let shutdown_tx = state.shutdown.clone();
    let shutdown = async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        let _ = shutdown_tx.send(true);
    };
    let version = env!("CARGO_PKG_VERSION");

    if let Some(path) = cfg.listen.as_deref().and_then(|l| l.strip_prefix("unix:")) {
        prepare_unix_socket(path)?;
        let listener = tokio::net::UnixListener::bind(path)?;
        // connect(2) needs write permission. A reverse proxy runs as another user, and systemd's
        // umask would otherwise leave the socket owner-only.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
        tracing::info!("{BRAND_SLUG} server {version} listening on unix:{path}");
        if cfg.bonjour {
            tracing::info!("bonjour is not advertised on a unix socket");
        }
        axum::serve(listener, router(state)).with_graceful_shutdown(shutdown).await?;
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(path);
        }
        return Ok(());
    }

    let listener = tokio::net::TcpListener::bind((cfg.bind.as_str(), cfg.port)).await?;
    let port = listener.local_addr()?.port();
    tracing::info!("{BRAND_SLUG} server {version} listening on {}:{port}", cfg.bind);
    let daemon = if cfg.bonjour { bonjour::advertise(port, &state.server_id) } else { None };
    axum::serve(listener, router(state)).with_graceful_shutdown(shutdown).await?;
    if let Some(d) = daemon {
        let _ = d.shutdown();
    }
    Ok(())
}

fn prepare_unix_socket(path: &str) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)?;
            tracing::info!("removed stale socket {path}");
        }
        Ok(_) => anyhow::bail!("{path} exists and is not a socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
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
