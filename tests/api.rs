use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use git_serve::{api, repo};
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

    fn router(&self) -> axum::Router {
        api::router(repo::sync(&self.url(), &self.cache).unwrap())
    }
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

    let app = f.router();
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
    drop(f.router());

    let second = f.commit(&[("a.txt", "2")], "second");
    f.git(&["branch", "-D", "gone"]);
    f.git(&["tag", "-d", "gone-tag"]);

    let app = f.router();
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
    let app = f.router();

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
    let app = f.router();

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
    let app = f.router();

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
    let app = f.router();

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
    let app = f.router();

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

    let err = repo::sync(&f.url(), &f.cache).unwrap_err();
    assert!(matches!(err, repo::SyncError::NotARepo { .. }), "{err}");
    assert!(Path::new(&stray).exists());
}

#[test]
fn cache_for_other_url_is_rejected() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "1")], "first");
    repo::sync(&f.url(), &f.cache).unwrap();

    let err = repo::sync("https://example.com/other.git", &f.cache).unwrap_err();
    assert!(matches!(err, repo::SyncError::UrlMismatch { .. }), "{err}");
}

#[tokio::test]
async fn unreachable_remote_serves_stale_cache() {
    let f = Fixture::new();
    f.commit(&[("a.txt", "cached")], "first");
    drop(f.router());
    std::fs::remove_dir_all(&f.upstream).unwrap();

    let app = f.router();
    assert_eq!(get(&app, "/files/a.txt").await.1, b"cached");
}
