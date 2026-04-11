use crate::routes;
use crate::state::AppState;
use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
};
use include_dir::{Dir, include_dir};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_http::services::{ServeDir, ServeFile};

pub const DEFAULT_API_PORT: u16 = 43_430;
static EMBEDDED_FRONTEND: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../frontend/static");

pub fn default_frontend_dir() -> PathBuf {
    std::env::var("CROOPOR_FRONTEND_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("frontend/static"))
}

pub fn build_router(state: AppState) -> Router {
    let frontend_dir = state.frontend_dir().to_path_buf();
    let index_path = frontend_dir.join("index.html");
    let api = routes::router(state);

    if frontend_dir.is_dir() && index_path.is_file() {
        let static_service =
            ServeDir::new(frontend_dir).not_found_service(ServeFile::new(index_path));
        api.fallback_service(static_service)
    } else {
        api.fallback(get(serve_embedded_frontend))
    }
}

#[derive(Debug)]
pub struct ServerHandle {
    pub addr: SocketAddr,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Error)]
pub enum ApiServerError {
    #[error("failed to bind listener: {0}")]
    Bind(#[from] io::Error),
}

pub async fn spawn_background(state: AppState) -> Result<ServerHandle, ApiServerError> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], DEFAULT_API_PORT))).await?;
    let addr = listener.local_addr()?;
    let router = build_router(state);

    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!("api server stopped: {error}");
        }
    });

    Ok(ServerHandle { addr, task })
}

async fn serve_embedded_frontend(uri: Uri) -> impl IntoResponse {
    let path = normalized_embedded_path(uri.path());
    let file = EMBEDDED_FRONTEND
        .get_file(&path)
        .or_else(|| EMBEDDED_FRONTEND.get_file("index.html"));

    match file {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                content_type_for_path(file.path().to_string_lossy().as_ref()),
            )
            .body(Body::from(file.contents()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn normalized_embedded_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return "index.html".to_string();
    }
    if EMBEDDED_FRONTEND.get_file(trimmed).is_some() {
        return trimmed.to_string();
    }
    "index.html".to_string()
}

fn content_type_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ogg" => "audio/ogg",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
