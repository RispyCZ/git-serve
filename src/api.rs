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
use crate::repo;

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

/// Cache policy of a response, from how its revision was named.
#[derive(Clone, Copy)]
enum Caching {
    /// Named by full object id: the answer can never change.
    Immutable,
    /// Named by a ref or an expression such as `HEAD~1`: clients revalidate with the `ETag`.
    Revalidate,
}

impl Caching {
    fn for_rev(rev: &str) -> Self {
        let full_id = matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_hexdigit());
        if full_id {
            Self::Immutable
        } else {
            Self::Revalidate
        }
    }

    fn cache_control(self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Self::Immutable => "public, max-age=31536000, immutable",
            Self::Revalidate => "no-cache",
        })
    }

    /// `body` with validators, or `304 Not Modified` when the client already holds it (`None`).
    fn respond(self, etag: &str, body: Option<impl IntoResponse>) -> Result<Response, AppError> {
        let headers = [
            (
                header::ETAG,
                HeaderValue::try_from(etag).map_err(AppError::internal)?,
            ),
            (header::CACHE_CONTROL, self.cache_control()),
        ];
        Ok(match body {
            Some(body) => (headers, body).into_response(),
            None => (StatusCode::NOT_MODIFIED, headers).into_response(),
        })
    }
}

/// The `If-None-Match` request header.
struct IfNoneMatch(Option<String>);

impl IfNoneMatch {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self(
            headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(ToOwned::to_owned),
        )
    }

    /// Weak comparison, as RFC 9110 requires for `If-None-Match`.
    fn matches(&self, etag: &str) -> bool {
        self.0.as_deref().is_some_and(|list| {
            list.split(',').map(str::trim).any(|candidate| {
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
            })
        })
    }
}

fn quoted(id: impl std::fmt::Display) -> String {
    format!("\"{id}\"")
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

async fn refs(State(mirror): AppState, headers: HeaderMap) -> Result<Response, AppError> {
    let if_none_match = IfNoneMatch::from_headers(&headers);
    let (etag, body) = with_repo(mirror, move |r| {
        let body = serde_json::to_vec(&repo::list_refs(r)?).map_err(AppError::internal)?;
        let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
        hasher.update(&body);
        let etag = quoted(hasher.try_finalize().map_err(AppError::internal)?);
        let body = (!if_none_match.matches(&etag)).then_some(body);
        Ok((etag, body))
    })
    .await?;
    Caching::Revalidate.respond(
        &etag,
        body.map(|b| ([(header::CONTENT_TYPE, "application/json")], b)),
    )
}

async fn tree_root(
    State(mirror): AppState,
    Query(q): Query<RevQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    tree_at(mirror, q.rev(), String::new(), &headers).await
}

async fn tree(
    State(mirror): AppState,
    Path(path): Path<String>,
    Query(q): Query<RevQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    tree_at(mirror, q.rev(), path, &headers).await
}

/// Directory listings of a commit never change, so its id is the `ETag`.
async fn tree_at(
    mirror: Arc<Mirror>,
    rev: String,
    path: String,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let caching = Caching::for_rev(&rev);
    let if_none_match = IfNoneMatch::from_headers(headers);
    let (etag, entries) = with_repo(mirror, move |r| {
        let commit = repo::resolve_commit(r, &rev)?;
        let etag = quoted(commit);
        if if_none_match.matches(&etag) {
            return Ok((etag, None));
        }
        Ok((etag, Some(repo::read_tree(r, commit, &path)?)))
    })
    .await?;
    caching.respond(&etag, entries.map(Json))
}

async fn file(
    State(mirror): AppState,
    Path(path): Path<String>,
    Query(q): Query<RevQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let rev = q.rev();
    let caching = Caching::for_rev(&rev);
    let if_none_match = IfNoneMatch::from_headers(&headers);
    let mime = HeaderValue::try_from(
        mime_guess::from_path(&path)
            .first_or_octet_stream()
            .as_ref(),
    )
    .map_err(AppError::internal)?;

    let (etag, data) = with_repo(mirror, move |r| {
        let id = repo::find_blob(r, repo::resolve_commit(r, &rev)?, &path)?;
        // Blobs are content-addressed, so a matching ETag means the client already has these bytes.
        let etag = quoted(id);
        if if_none_match.matches(&etag) {
            return Ok((etag, None));
        }
        Ok((etag, Some(repo::read_blob(r, id)?)))
    })
    .await?;
    caching.respond(&etag, data.map(|d| ([(header::CONTENT_TYPE, mime)], d)))
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(rename = "ref")]
    rev: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
}

/// History below a commit never changes, so its id is the `ETag`; path and limit are in the URL.
async fn commits(
    State(mirror): AppState,
    Query(q): Query<LogQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(AppError::BadRequest(format!(
            "limit must be between 1 and {MAX_LOG_LIMIT}"
        )));
    }
    let rev = q.rev.unwrap_or_else(|| "HEAD".to_owned());
    let caching = Caching::for_rev(&rev);
    let if_none_match = IfNoneMatch::from_headers(&headers);
    let (etag, commits) = with_repo(mirror, move |r| {
        let start = repo::resolve_commit(r, &rev)?;
        let etag = quoted(start);
        if if_none_match.matches(&etag) {
            return Ok((etag, None));
        }
        Ok((etag, Some(repo::log(r, start, q.path.as_deref(), limit)?)))
    })
    .await?;
    caching.respond(&etag, commits.map(Json))
}
