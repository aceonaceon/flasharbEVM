//! Read-only status dashboard: one HTML page and one JSON endpoint.
//! It opens the ledger read-only and exposes no way to change anything.

use crate::config::Config;
use crate::dashboard::{build, Live};
use crate::ledger::open_read_only;
use crate::rpc::RPC_ERRORS;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub static CHAIN_HEAD: AtomicU64 = AtomicU64::new(0);
pub static LAST_TS: AtomicU64 = AtomicU64::new(0);
pub static RECORDING: AtomicBool = AtomicBool::new(false);

const INDEX: &str = include_str!("../web/index.html");

struct AppState {
    db: PathBuf,
    cfg: Config,
}

pub fn router(db: PathBuf, cfg: Config) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/summary", get(summary))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(Arc::new(AppState { db, cfg }))
}

pub async fn serve(bind: &str, db: PathBuf, cfg: Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(bind, "dashboard listening");
    axum::serve(listener, router(db, cfg)).await?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX))
}

async fn summary(State(st): State<Arc<AppState>>) -> Response {
    let live = Live {
        chain_head: CHAIN_HEAD.load(Ordering::Relaxed),
        last_ts: LAST_TS.load(Ordering::Relaxed),
        recording: RECORDING.load(Ordering::Relaxed),
        rpc_errors: RPC_ERRORS.load(Ordering::Relaxed),
        now: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
    };
    let res = tokio::task::spawn_blocking(move || {
        let conn = open_read_only(&st.db)?;
        build(&conn, &st.cfg, &live)
    })
    .await;
    match res {
        Ok(Ok(s)) => ([(header::CACHE_CONTROL, "no-store")], Json(s)).into_response(),
        Ok(Err(e)) => (StatusCode::SERVICE_UNAVAILABLE, format!("ledger not ready: {e}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
