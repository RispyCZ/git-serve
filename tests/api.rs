use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::IntoResponse;
use git_serve::api;
use git_serve::config::MirrorConfig;
use git_serve::mirror::{Mirror, SyncError};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

/// A non-bare upstream repository driven by the git CLI, plus an empty cache dir next to it.
struct Fixture {
    _tmp: TempDir,
    upstream: PathBuf,
    cache: PathBuf,
    commits: Cell<u32>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let upstream = root.join("upstream");
        let cache = root.join("cache");
        std::fs::create_dir(&upstream).unwrap();
        let f = Self {
            _tmp: tmp,
            upstream,
            cache,
            commits: Cell::new(0),
        };
        f.git(&["init", "-q", "-b", "main"]);
        f
    }

    fn url(&self) -> String {
        format!("file://{}", self.upstream.display())
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .current_dir(&self.upstream)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(&self, files: &[(&str, &str)], message: &str) -> String {
        for (path, content) in files {
            let path = self.upstream.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        // Distinct timestamps keep commit-time ordering deterministic.
        let n = self.commits.get() + 1;
        self.commits.set(n);
        let date = format!("{} +0000", 1_700_000_000 + n * 60);
        self.git(&["add", "-A"]);
        let out = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(&self.upstream)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.git(&["rev-parse", "HEAD"])
    }

    fn config(&self) -> MirrorConfig {
        MirrorConfig::new(self.url(), &self.cache)
    }

    /// A router whose first sync has finished.
    async fn router(&self) -> axum::Router {
        start(self.config()).await
    }
}

async fn start(config: MirrorConfig) -> axum::Router {
    let mirror = Arc::new(Mirror::open(config).unwrap());
    mirror.spawn();
    mirror.ensure_fresh(false).await;
    api::router(mirror)
}

fn open_err(config: MirrorConfig) -> SyncError {
    match Mirror::open(config) {
        Ok(_) => panic!("expected Mirror::open to fail"),
        Err(e) => e,
    }
}

async fn post(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::post(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    (
        status,
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, body) = get(app, uri).await;
    (status, serde_json::from_slice(&body).unwrap())
}

fn names(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn clones_into_empty_cache_and_lists_refs() {
    let f = Fixture::new();
    let first = f.commit(&[("README.md", "hello\n")], "first");
    f.git(&["tag", "-a", "v1", "-m", "release"]);
    f.git(&["branch", "feature"]);

    let app = f.router().await;
    let (status, refs) = get_json(&app, "/refs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(refs["head"], "main");
    assert_eq!(names(&refs["branches"]), ["feature", "main"]);
    assert_eq!(names(&refs["tags"]), ["v1"]);
    // Annotated tag is peeled to the commit it points at.
    assert_eq!(refs["tags"][0]["target"], first);
}

#[tokio::test]
async fn existing_cache_fetches_new_commits_and_prunes_deleted_refs() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "1")], "first");
    f.git(&["branch", "gone"]);
    f.git(&["tag", "gone-tag"]);
    drop(f.router().await);

    let second = f.commit(&[("a.txt", "2")], "second");
    f.git(&["branch", "-D", "gone"]);
    f.git(&["tag", "-d", "gone-tag"]);

    let app = f.router().await;
    let (_, refs) = get_json(&app, "/refs").await;
    assert_eq!(names(&refs["branches"]), ["main"]);
    assert_eq!(refs["branches"][0]["target"], second);
    assert!(refs["tags"].as_array().unwrap().is_empty());
    assert_eq!(get(&app, "/files/a.txt").await.1, b"2");
}

#[tokio::test]
async fn files_resolve_any_ref() {
    let f = Fixture::new();
    let old = f.commit(&[("src/lib.rs", "old")], "first");
    f.commit(&[("src/lib.rs", "new")], "second");
    let app = f.router().await;

    assert_eq!(get(&app, "/files/src/lib.rs").await.1, b"new");
    assert_eq!(
        get(&app, &format!("/files/src/lib.rs?ref={old}")).await.1,
        b"old"
    );
    assert_eq!(get(&app, "/files/src/lib.rs?ref=HEAD~1").await.1, b"old");

    assert_eq!(
        get(&app, "/files/missing.rs").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&app, "/files/src").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        get(&app, "/files/src/lib.rs?ref=nope").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn file_etag_is_blob_id_and_honours_if_none_match() {
    let f = Fixture::new();
    f.commit(&[("data.json", "{}")], "first");
    let blob = f.git(&["rev-parse", "HEAD:data.json"]);
    let app = f.router().await;

    let res = app
        .clone()
        .oneshot(
            Request::get("/files/data.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json");
    let etag = res.headers()[header::ETAG].clone();
    assert_eq!(etag, format!("\"{blob}\"").as_str());

    let res = app
        .oneshot(
            Request::get("/files/data.json")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn tree_lists_entries_with_kinds() {
    let f = Fixture::new();
    f.commit(
        &[("README.md", "hello"), ("src/main.rs", "fn main() {}")],
        "first",
    );
    std::os::unix::fs::symlink("README.md", f.upstream.join("link")).unwrap();
    f.commit(&[], "symlink");
    let app = f.router().await;

    let (status, root) = get_json(&app, "/tree").await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<(&str, &str)> = root
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [("README.md", "file"), ("link", "symlink"), ("src", "dir")]
    );
    assert_eq!(root[0]["size"], 5);
    assert!(root[2].get("size").is_none());

    let (_, src) = get_json(&app, "/tree/src/").await;
    assert_eq!(names(&src), ["main.rs"]);
    assert_eq!(
        get(&app, "/tree/README.md").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(get(&app, "/tree/nope").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn commits_filter_by_path_and_limit() {
    let f = Fixture::new();
    let c1 = f.commit(&[("a.txt", "1"), ("b.txt", "1")], "one");
    let c2 = f.commit(&[("b.txt", "2")], "two");
    let c3 = f.commit(&[("a.txt", "3")], "three");
    let app = f.router().await;

    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect()
    };

    let (_, all) = get_json(&app, "/commits").await;
    assert_eq!(ids(&all), [c3.as_str(), c2.as_str(), c1.as_str()]);
    assert_eq!(all[0]["message"], "three\n");
    assert_eq!(all[0]["parents"][0], c2);

    assert_eq!(
        ids(&get_json(&app, "/commits?path=a.txt").await.1),
        [c3.as_str(), c1.as_str()]
    );
    assert_eq!(
        ids(&get_json(&app, "/commits?path=b.txt").await.1),
        [c2.as_str(), c1.as_str()]
    );
    assert_eq!(
        ids(&get_json(&app, "/commits?limit=1").await.1),
        [c3.as_str()]
    );
    assert_eq!(
        get(&app, "/commits?limit=0").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn commits_path_filter_skips_merge_that_took_one_side() {
    let f = Fixture::new();
    let base = f.commit(&[("a.txt", "1"), ("b.txt", "1")], "base");
    f.git(&["checkout", "-q", "-b", "side"]);
    let side = f.commit(&[("a.txt", "2")], "side");
    f.git(&["checkout", "-q", "main"]);
    f.commit(&[("b.txt", "2")], "main");
    f.git(&["merge", "-q", "--no-edit", "side"]);
    let merge = f.git(&["rev-parse", "HEAD"]);
    let app = f.router().await;

    let (_, log) = get_json(&app, "/commits?path=a.txt").await;
    let ids: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&merge.as_str()), "{ids:?}");
    assert_eq!(ids, [side.as_str(), base.as_str()]);
}

#[test]
fn non_repo_cache_dir_fails_without_touching_it() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "1")], "first");
    std::fs::create_dir(&f.cache).unwrap();
    let stray = f.cache.join("stray.txt");
    std::fs::write(&stray, "keep me").unwrap();

    let err = open_err(f.config());
    assert!(matches!(err, SyncError::NotARepo { .. }), "{err}");
    assert!(Path::new(&stray).exists());
}

#[tokio::test]
async fn cache_for_other_url_is_rejected() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "1")], "first");
    drop(f.router().await);

    let err = open_err(MirrorConfig::new("https://example.com/other.git", &f.cache));
    assert!(matches!(err, SyncError::UrlMismatch { .. }), "{err}");
}

#[test]
fn password_in_url_is_rejected_without_echoing_it() {
    let f = Fixture::new();
    let err = open_err(MirrorConfig::new(
        "https://user:s3cret@example.com/repo.git",
        &f.cache,
    ));
    assert!(matches!(err, SyncError::PasswordInUrl), "{err}");
    assert!(!err.to_string().contains("s3cret"));
}

#[tokio::test]
async fn unreachable_remote_serves_stale_cache() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "cached")], "first");
    drop(f.router().await);
    std::fs::remove_dir_all(&f.upstream).unwrap();

    let app = f.router().await;
    assert_eq!(get(&app, "/files/a.txt").await.1, b"cached");
    let (status, sync) = post(&app, "/sync").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        sync["error"].as_str().unwrap().contains("git fetch"),
        "{sync}"
    );
    assert_eq!(get(&app, "/files/a.txt").await.1, b"cached");
}

#[tokio::test]
async fn failed_clone_is_unavailable_until_it_succeeds() {
    let f = Fixture::new();
    let mut config = f.config();
    config.ttl = Duration::ZERO;
    config.url = format!("file://{}", f.upstream.join("missing").display());
    let mirror = Arc::new(Mirror::open(config).unwrap());
    let app = api::router(Arc::clone(&mirror));

    // Nothing is cloned before the worker runs.
    let (status, health) = get_json(&app, "/healthz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["error"], "repository is being cloned");

    mirror.spawn();
    let res = app
        .clone()
        .oneshot(Request::get("/refs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(res.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(
        get(&app, "/healthz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn requests_within_ttl_are_served_from_cache_until_forced_sync() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "1")], "first");
    let app = f.router().await;
    assert_eq!(get_json(&app, "/healthz").await.0, StatusCode::OK);

    f.commit(&[("a.txt", "2")], "second");
    assert_eq!(get(&app, "/files/a.txt").await.1, b"1");

    let (status, sync) = post(&app, "/sync").await;
    assert_eq!(status, StatusCode::OK);
    assert!(sync["error"].is_null(), "{sync}");
    assert_eq!(sync["last_success_at"], sync["last_sync_at"]);
    assert_eq!(get(&app, "/files/a.txt").await.1, b"2");
    assert_eq!(get_json(&app, "/sync").await.1, sync);
}

#[tokio::test]
async fn stale_cache_syncs_before_answering() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "1")], "first");
    let mut config = f.config();
    config.ttl = Duration::ZERO;
    let app = start(config).await;
    assert_eq!(get(&app, "/files/a.txt").await.1, b"1");

    // The open repository picks up the pack the fetch added.
    f.commit(&[("a.txt", "2")], "second");
    assert_eq!(get(&app, "/files/a.txt").await.1, b"2");
}

#[tokio::test]
async fn head_follows_upstream_default_branch() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "main")], "first");
    f.git(&["branch", "other"]);
    let app = f.router().await;
    assert_eq!(get_json(&app, "/refs").await.1["head"], "main");

    // Renamed default branch: picked up by the sync that sees the refs change.
    f.git(&["checkout", "-q", "-b", "trunk"]);
    f.commit(&[("a.txt", "trunk")], "on trunk");
    f.git(&["branch", "-D", "main"]);
    assert_eq!(post(&app, "/sync").await.0, StatusCode::OK);
    assert_eq!(get_json(&app, "/refs").await.1["head"], "trunk");
    assert_eq!(get(&app, "/files/a.txt").await.1, b"trunk");
    drop(app);

    // Switched to an existing branch while no refs moved: picked up after a restart.
    f.git(&["symbolic-ref", "HEAD", "refs/heads/other"]);
    let app = f.router().await;
    assert_eq!(get_json(&app, "/refs").await.1["head"], "other");
}

/// Answers every request with 401 until it carries credentials, then 404, recording them.
async fn auth_recording_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let app = axum::Router::new().fallback(move |headers: HeaderMap| {
        let record = Arc::clone(&record);
        async move {
            match headers.get(header::AUTHORIZATION) {
                Some(auth) => {
                    record
                        .lock()
                        .unwrap()
                        .push(auth.to_str().unwrap().to_owned());
                    StatusCode::NOT_FOUND.into_response()
                }
                None => (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Basic realm=\"test\"")],
                )
                    .into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/repo.git"), seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn token_is_sent_through_credential_helper() {
    let f = Fixture::new();
    let (url, seen) = auth_recording_server().await;
    let mut config = MirrorConfig::new(url, &f.cache);
    config.token = Some("s3cret".to_owned());
    let mirror = Arc::new(Mirror::open(config).unwrap());
    mirror.spawn();

    let status = mirror.ensure_fresh(false).await;
    assert!(status.error.is_some());
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["Basic eC1hY2Nlc3MtdG9rZW46czNjcmV0"] // x-access-token:s3cret
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hung_upstream_times_out() {
    let f = Fixture::new();
    // Accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let mut config = MirrorConfig::new(format!("http://{addr}/repo.git"), &f.cache);
    config.git_timeout = Duration::from_secs(1);
    let mirror = Arc::new(Mirror::open(config).unwrap());
    mirror.spawn();

    let started = Instant::now();
    let status = mirror.ensure_fresh(false).await;
    assert_eq!(status.error.as_deref(), Some("git clone timed out"));
    // Well under the SIGTERM grace period: the transport helper was signalled along with git.
    assert!(started.elapsed() < Duration::from_secs(4));
}
