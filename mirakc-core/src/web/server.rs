use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::Request;
use hyper::body::Incoming;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use hyper_util::server;
use tokio::net::TcpListener;
use tokio::net::UnixListener;
use tokio::time::sleep;
use tower::Service;

use actlet::Spawn;

use crate::config::Config;
use crate::error::Error;

use super::peer_info::PeerInfo;

const LISTENER_RESTART_DELAY: Duration = Duration::from_secs(5);

pub(super) async fn serve<W>(config: Arc<Config>, app: Router, spawner: W) -> Result<(), Error>
where
    W: Clone + Send + Spawn + 'static,
{
    let mut handles = vec![];
    for addr in config.server.http_addrs() {
        let (handle, _) = spawner.spawn_task(http(addr, app.clone(), spawner.clone()));
        handles.push(handle);
    }
    for path in config.server.uds_paths() {
        let (handle, _) = spawner.spawn_task(uds(path.to_owned(), app.clone(), spawner.clone()));
        handles.push(handle);
    }
    for handle in handles.into_iter() {
        let _ = handle.await;
    }
    Ok(())
}

// See //examples/serve-with-hyper in tokio-rs/axum.
// See //examples/unix-domain-socket in tokio-rs/axum.

macro_rules! listen {
    ($listener:expr, $app:expr, $spawner:expr) => {{
        let mut make_service = $app.into_make_service_with_connect_info::<PeerInfo>();
        loop {
            let (socket, _remote_addr) = match $listener.accept().await {
                Ok(result) => result,
                Err(err) => {
                    tracing::error!(?err, "Failed to accept connection");
                    break;
                }
            };
            let tower_service = match make_service.call(&socket).await {
                Ok(service) => service,
                Err(err) => {
                    tracing::error!(?err, "Failed to create service for connection");
                    continue;
                }
            };
            $spawner.spawn_task(async move {
                let socket = TokioIo::new(socket);
                let hyper_service =
                    hyper::service::service_fn(move |request: Request<Incoming>| {
                        tower_service.clone().call(request)
                    });
                // TODO(refactor): use Spawner (or wrapper) as hyper::rt::Executor.
                // TokioExecutor::execute() calls tokio::spawn().
                let executor = TokioExecutor::new();
                if let Err(err) = server::conn::auto::Builder::new(executor)
                    .serve_connection(socket, hyper_service)
                    .await
                {
                    tracing::debug!(?err, "Failed to serve connection");
                }
            });
        }
    }};
}

async fn http<W>(addr: std::net::SocketAddr, app: Router, spawner: W)
where
    W: Clone + Send + Spawn,
{
    loop {
        match TcpListener::bind(&addr).await {
            Ok(listener) => {
                tracing::info!(%addr, "HTTP listener started");
                listen!(listener, app.clone(), spawner.clone());
            }
            Err(err) => {
                tracing::error!(?err, %addr, "Failed to bind HTTP listener");
            }
        }

        tracing::warn!(
            %addr,
            delay_secs = LISTENER_RESTART_DELAY.as_secs(),
            "Restarting HTTP listener after delay"
        );
        sleep(LISTENER_RESTART_DELAY).await;
    }
}

async fn uds<W>(path: PathBuf, app: Router, spawner: W)
where
    W: Clone + Send + Spawn,
{
    loop {
        match bind_uds(&path).await {
            Ok(listener) => {
                tracing::info!(path = %path.display(), "Unix-domain socket listener started");
                listen!(listener, app.clone(), spawner.clone());
            }
            Err(err) => {
                tracing::error!(?err, path = %path.display(), "Failed to bind Unix-domain socket listener");
            }
        }

        tracing::warn!(
            path = %path.display(),
            delay_secs = LISTENER_RESTART_DELAY.as_secs(),
            "Restarting Unix-domain socket listener after delay"
        );
        sleep(LISTENER_RESTART_DELAY).await;
    }
}

async fn bind_uds(path: &std::path::Path) -> std::io::Result<UnixListener> {
    // Remove the socket if it exists.
    let _ = tokio::fs::remove_file(path).await;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    UnixListener::bind(path)
}
