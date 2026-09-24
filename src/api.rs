use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::mirror::{Mirror, Status};
use crate::repo::{self, CommitInfo, Refs, TreeEntry};

const DEFAULT_LOG_LIMIT: usize = 50;
const MAX_LOG_LIMIT: usize = 500;
const OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

pub fn router(mirror: Arc<Mirror>) -> Router {
    let content = Router::new()
        .route("/refs", get(refs))
        .route("/tree", get(tree_root))
        .route("/tree/{*path}", get(tree))
        .route("/files/{*path}", get(file))
        .route("/commits", get(commits))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&mirror),
            ensure_fresh,
        ));
    Router::new()
        .merge(content)
        .route("/healthz", get(healthz))
        .route("/sync", get(sync_status).post(sync))
        .with_state(mirror)
}

type AppState = State<Arc<Mirror>>;

/// Brings the cache up to date with the upstream first when the last sync is older than the TTL.
async fn ensure_fresh(State(mirror): AppState, req: Request, next: Next) -> Response {
    mirror.ensure_fresh(false).await;
    next.run(req).await
}

fn not_ready(mirror: &Mirror) -> AppError {
    AppError::Unavailable(match mirror.status().error {
        Some(e) => format!("repository is not available: {e}"),
        None => "repository is being cloned".to_owned(),
    })
}

/// Ready once the cache holds a repository, which may be older than the upstream.
async fn healthz(State(mirror): AppState) -> Result<Json<serde_json::Value>, AppError> {
    match mirror.repo() {
        Some(_) => Ok(Json(json!({ "status": "ok" }))),
        None => Err(not_ready(&mirror)),
    }
}

async fn sync_status(State(mirror): AppState) -> Json<Status> {
    Json(mirror.status())
}

/// Syncs with the upstream now, e.g. from a push webhook, and answers with the outcome.
async fn sync(State(mirror): AppState) -> (StatusCode, Json<Status>) {
    let status = mirror.ensure_fresh(true).await;
    let code = if status.error.is_some() {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::OK
    };
    (code, Json(status))
}

/// Runs blocking gix work off the async executor on a thread-local repository handle.
async fn with_repo<T, F>(mirror: Arc<Mirror>, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&gix::Repository) -> Result<T, AppError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let repo = mirror.repo().ok_or_else(|| not_ready(&mirror))?;
        let mut repo = repo.to_thread_local();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        f(&repo)
    })
    .await
    .map_err(AppError::internal)?
}

#[derive(Deserialize)]
struct RevQuery {
    #[serde(rename = "ref")]
    rev: Option<String>,
}

impl RevQuery {
    fn rev(self) -> String {
        self.rev.unwrap_or_else(|| "HEAD".to_owned())
    }
}

async fn refs(State(repo): AppState) -> Result<Json<Refs>, AppError> {
    with_repo(repo, repo::list_refs).await.map(Json)
}

async fn tree_root(
    State(repo): AppState,
    Query(q): Query<RevQuery>,
) -> Result<Json<Vec<TreeEntry>>, AppError> {
    let rev = q.rev();
    with_repo(repo, move |r| repo::read_tree(r, &rev, ""))
        .await
        .map(Json)
}

async fn tree(
    State(repo): AppState,
    Path(path): Path<String>,
    Query(q): Query<RevQuery>,
) -> Result<Json<Vec<TreeEntry>>, AppError> {
    let rev = q.rev();
    with_repo(repo, move |r| repo::read_tree(r, &rev, &path))
        .await
        .map(Json)
}

async fn file(
    State(repo): AppState,
    Path(path): Path<String>,
    Query(q): Query<RevQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let rev = q.rev();
    let if_none_match = headers.get(header::IF_NONE_MATCH).cloned();
    let mime = mime_guess::from_path(&path).first_or_octet_stream();

    let (etag, data) = with_repo(repo, move |r| {
        let id = repo::find_blob(r, &rev, &path)?;
        let etag = HeaderValue::try_from(format!("\"{id}\"")).map_err(AppError::internal)?;
        // Blobs are content-addressed, so a matching ETag means the client already has these bytes.
        let data = if if_none_match.as_ref() == Some(&etag) {
            None
        } else {
            Some(repo::read_blob(r, id)?)
        };
        Ok((etag, data))
    })
    .await?;

    Ok(match data {
        None => (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response(),
        Some(data) => (
            [
                (header::ETAG, etag),
                (
                    header::CONTENT_TYPE,
                    HeaderValue::try_from(mime.as_ref()).map_err(AppError::internal)?,
                ),
            ],
            data,
        )
            .into_response(),
    })
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(rename = "ref")]
    rev: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
}

async fn commits(
    State(repo): AppState,
    Query(q): Query<LogQuery>,
) -> Result<Json<Vec<CommitInfo>>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(AppError::BadRequest(format!(
            "limit must be between 1 and {MAX_LOG_LIMIT}"
        )));
    }
    let rev = q.rev.unwrap_or_else(|| "HEAD".to_owned());
    with_repo(repo, move |r| repo::log(r, &rev, q.path.as_deref(), limit))
        .await
        .map(Json)
}
