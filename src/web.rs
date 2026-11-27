//! Read-only status dashboard: one HTML page and one JSON endpoint.
//! It opens the ledger read-only and exposes no way to change anything.

use crate::config::Config;
use crate::dashboard::{build, Live};
use crate::ledger::open_read_only;
use crate::rpc::RPC_ERRORS;
use axum::{
    extract::{Query, State},
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
    http: reqwest::Client,
}

#[derive(serde::Deserialize)]
struct ChainQuery {
    chain: Option<String>,
}

pub fn router(db: PathBuf, cfg: Config) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/summary", get(summary))
        .route("/api/chains", get(chains))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(Arc::new(AppState { db, cfg, http: reqwest::Client::new() }))
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

/// This chain first, then the peers configured in `[web] peers`.
async fn chains(State(st): State<Arc<AppState>>) -> Json<Vec<String>> {
    let mut v = vec![st.cfg.chain.name.clone()];
    v.extend(st.cfg.web.peers.iter().map(|p| p.name.clone()));
    Json(v)
}

async fn summary(State(st): State<Arc<AppState>>, Query(q): Query<ChainQuery>) -> Response {
    if let Some(name) = q.chain.as_deref().filter(|n| *n != st.cfg.chain.name) {
        let Some(peer) = st.cfg.web.peers.iter().find(|p| p.name == name) else {
            return (StatusCode::NOT_FOUND, "unknown chain").into_response();
        };
        let url = format!("{}/api/summary", peer.url.trim_end_matches('/'));
        let res = st.http.get(&url).timeout(std::time::Duration::from_secs(5)).send().await;
        return match res {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) => ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], b.to_vec())
                    .into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
            },
            Ok(r) => (StatusCode::BAD_GATEWAY, format!("{name}: {}", r.status())).into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("{name} not reachable: {e}")).into_response(),
        };
    }
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
