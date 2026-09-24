use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use gix::ThreadSafeRepository;
use serde::Deserialize;

use crate::error::AppError;
use crate::repo::{self, CommitInfo, Refs, TreeEntry};

const DEFAULT_LOG_LIMIT: usize = 50;
const MAX_LOG_LIMIT: usize = 500;
const OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

pub fn router(repo: ThreadSafeRepository) -> Router {
    Router::new()
        .route("/refs", get(refs))
        .route("/tree", get(tree_root))
        .route("/tree/{*path}", get(tree))
        .route("/files/{*path}", get(file))
        .route("/commits", get(commits))
        .with_state(Arc::new(repo))
}

type AppState = State<Arc<ThreadSafeRepository>>;

/// Runs blocking gix work off the async executor on a thread-local repository handle.
async fn with_repo<T, F>(repo: Arc<ThreadSafeRepository>, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&gix::Repository) -> Result<T, AppError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
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
