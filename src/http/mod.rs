//! The query endpoint.
//!
//! Four routes, all versioned under `/v1`:
//!
//! | route | purpose |
//! | --- | --- |
//! | `GET /v1/status` | directory generation, so a client knows what to fetch |
//! | `GET /v1/directory` | full directory blob, for bootstrap and after a rebuild |
//! | `GET /v1/directory/tail?from=N` | append-only delta from `N` |
//! | `POST /v1/query` | one PIR query in, one response out |
//!
//! The browser portal normally calls this API through a same-origin local proxy.
//! Nothing here is confidential: hiding the address is the PIR query's job.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use eth_pir::EthPirResponder;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::directory;
use response::{blob, json, no_store, problem, text, unavailable};

mod batch;
mod limit;
mod response;
mod static_files;

pub use batch::BatchConfig;
pub use limit::RateLimit;

const MAX_QUERY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct Api {
    batcher: batch::Batcher,
    limiter: std::sync::Arc<limit::RateLimiter>,
    directory: directory::Handle,
    progress: crate::progress::Handle,
}

pub struct Endpoint {
    pub listen: SocketAddr,
    pub responder: EthPirResponder,
    pub directory: directory::Handle,
    pub progress: crate::progress::Handle,
    pub web: Option<std::path::PathBuf>,
    pub batch: BatchConfig,
    pub rate: RateLimit,
}

pub async fn spawn(endpoint: Endpoint) -> Result<JoinHandle<Result<()>>> {
    let bound = bind(endpoint).await?;
    Ok(tokio::spawn(async move {
        let result = run(bound).await;
        if let Err(e) = &result {
            tracing::error!("query endpoint stopped: {e:#}");
        }
        result
    }))
}

struct Bound {
    listen: SocketAddr,
    listener: TcpListener,
    app: Router,
}

async fn bind(endpoint: Endpoint) -> Result<Bound> {
    let Endpoint {
        listen,
        responder,
        directory,
        progress,
        web,
        batch: batch_cfg,
        rate,
    } = endpoint;

    anyhow::ensure!(batch_cfg.max >= 1, "--max-batch must be at least 1");
    anyhow::ensure!(
        batch_cfg.queue_depth >= 1,
        "--queue-depth must be at least 1"
    );
    anyhow::ensure!(rate.burst >= 1, "--rate-burst must be at least 1");
    let api = Api {
        batcher: batch::Batcher::spawn(responder, batch_cfg),
        limiter: std::sync::Arc::new(limit::RateLimiter::new(rate)),
        directory,
        progress,
    };

    let mut app = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/directory", get(full_directory))
        .route("/v1/directory/tail", get(tail))
        .route("/v1/query", post(query))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_QUERY_BYTES))
        .with_state(api);

    if let Some(dir) = web {
        app = app.merge(static_files::router(dir));
    }

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    Ok(Bound {
        listen,
        listener,
        app,
    })
}

async fn run(bound: Bound) -> Result<()> {
    let Bound {
        listen,
        listener,
        app,
    } = bound;
    tracing::info!("query endpoint listening on http://{listen}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("http server")?;
    Ok(())
}

async fn status(State(api): State<Api>) -> Response {
    let Some(dir) = directory::current(&api.directory) else {
        return unavailable();
    };
    let chain = api.progress.sample();
    let body = format!(
        concat!(
            r#"{{"version":{},"len":{},"mphfLen":{},"tailLen":{},"#,
            r#""directoryBytes":{},"mphfBytes":{},"tailBytes":{},"#,
            r#""cursor":{},"tip":{},"lagBlocks":{},"lastSyncAgeSecs":{}}}"#
        ),
        dir.version,
        dir.len,
        dir.mphf_len,
        dir.tail_len,
        dir.full.len(),
        dir.mphf_bytes,
        dir.tail_bytes,
        chain.cursor,
        chain.tip,
        chain.lag_blocks,
        // null, not 0: "never synced" must not read as "synced just now".
        chain
            .age_secs
            .map_or_else(|| "null".to_string(), |a| a.to_string()),
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, json()),
            (header::CACHE_CONTROL, no_store()),
        ],
        body,
    )
        .into_response()
}

async fn full_directory(State(api): State<Api>) -> Response {
    let Some(dir) = directory::current(&api.directory) else {
        return unavailable();
    };
    blob(dir.full.clone())
}

async fn tail(State(api): State<Api>, RawQuery(raw): RawQuery) -> Response {
    let Some(dir) = directory::current(&api.directory) else {
        return unavailable();
    };
    let from = match parse_from(raw.as_deref()) {
        Ok(from) => from,
        Err(e) => return problem(StatusCode::BAD_REQUEST, e),
    };
    match dir.tail(from) {
        Ok(bytes) => blob(bytes),
        Err(e) => problem(StatusCode::BAD_REQUEST, &format!("tail: {e}")),
    }
}

/// `?from=N`, defaulting to 0 when absent.
fn parse_from(raw: Option<&str>) -> Result<usize, &'static str> {
    let Some(raw) = raw else { return Ok(0) };
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "from" {
            return value.parse().map_err(|_| "from must be a whole number");
        }
    }
    Ok(0)
}

async fn query(
    State(api): State<Api>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return problem(StatusCode::BAD_REQUEST, "empty query");
    }
    if let Err(wait) = api.limiter.check(peer.ip()) {
        return too_many(wait);
    }

    match api.batcher.submit(body.to_vec()).await {
        Ok(Ok(bytes)) => blob(bytes),
        Ok(Err(e)) => problem(StatusCode::BAD_REQUEST, &format!("query rejected: {e}")),
        Err(rejected) => problem(StatusCode::SERVICE_UNAVAILABLE, rejected.message()),
    }
}

fn too_many(wait: std::time::Duration) -> Response {
    let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            (header::CONTENT_TYPE, text()),
            (header::RETRY_AFTER, HeaderValue::from(secs)),
        ],
        format!("rate limit reached; retry in {secs}s\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::parse_from;

    #[test]
    fn from_defaults_to_zero_and_parses_when_present() {
        assert_eq!(parse_from(None), Ok(0));
        assert_eq!(parse_from(Some("")), Ok(0));
        assert_eq!(parse_from(Some("from=42")), Ok(42));
        assert_eq!(parse_from(Some("x=1&from=7")), Ok(7));
        assert_eq!(parse_from(Some("other=3")), Ok(0));
        assert!(parse_from(Some("from=-1")).is_err());
        assert!(parse_from(Some("from=abc")).is_err());
    }
}
