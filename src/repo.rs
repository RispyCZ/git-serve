use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::{ObjectId, Repository, ThreadSafeRepository};
use serde::Serialize;

use crate::error::AppError;

/// The cache is a bare mirror: remote branches and tags map 1:1 onto local refs.
const REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];
const REMOTE_NAME: &str = "origin";

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("cannot read cache dir {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("cache dir {path} is not empty and not a git repository: {source}")]
    NotARepo {
        path: String,
        source: Box<gix::open::Error>,
    },
    #[error("cache dir remote {REMOTE_NAME:?} is {found:?}, expected {expected:?}")]
    UrlMismatch { expected: String, found: String },
    #[error("{0}")]
    Git(String),
}

impl SyncError {
    fn git(e: impl std::fmt::Display) -> Self {
        Self::Git(e.to_string())
    }
}

/// Clones `url` into `dir` when it is missing or empty, otherwise fetches into the existing mirror.
///
/// A failed fetch is logged and the stale cache is served; a failed clone is an error.
pub fn sync(url: &str, dir: &Path) -> Result<ThreadSafeRepository, SyncError> {
    let is_empty = match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(source) => {
            return Err(SyncError::Io {
                path: dir.display().to_string(),
                source,
            });
        }
    };

    if is_empty {
        tracing::info!(url, dir = %dir.display(), "cloning");
        clone(url, dir)?;
    }

    let repo = gix::open(dir).map_err(|source| SyncError::NotARepo {
        path: dir.display().to_string(),
        source: Box::new(source),
    })?;
    if !is_empty {
        check_origin(&repo, url)?;
        tracing::info!(url, dir = %dir.display(), "fetching");
        if let Err(e) = fetch(dir) {
            tracing::warn!(error = %e, "fetch failed, serving cached repository");
        }
    }
    Ok(repo.into_sync())
}

/// Runs `cmd`, turning a non-zero exit into an error carrying git's stderr.
fn run(cmd: &mut Command) -> Result<(), SyncError> {
    // Network sync goes through the git binary for its transports (ssh, credential helpers, proxies).
    // Disabling the prompt makes missing credentials fail instead of blocking startup.
    let out = cmd
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| SyncError::Git(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(SyncError::Git(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(())
}

/// A bare clone already maps branches and tags onto local refs, matching `REFSPECS`.
fn clone(url: &str, dir: &Path) -> Result<(), SyncError> {
    run(Command::new("git")
        .args(["clone", "--bare", "--quiet", "--origin", REMOTE_NAME, "--"])
        .arg(url)
        .arg(dir))
}

/// Fetches all branches and tags, pruning local refs that no longer exist on the remote.
fn fetch(dir: &Path) -> Result<(), SyncError> {
    run(Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["fetch", "--prune", "--quiet", REMOTE_NAME])
        .args(REFSPECS))
}

/// Clone stores local paths canonicalized, so compare local URLs by canonical path.
fn canonical(mut url: gix::Url) -> gix::Url {
    if url.scheme == gix::url::Scheme::File
        && let Ok(path) = gix::path::try_from_bstr(url.path.as_bstr())
        && let Ok(path) = std::fs::canonicalize(path)
    {
        url.path = gix::path::into_bstr(path).into_owned();
    }
    url
}

fn check_origin(repo: &Repository, url: &str) -> Result<(), SyncError> {
    let expected = gix::url::parse(url.as_bytes().as_bstr()).map_err(SyncError::git)?;
    // Read the configured value directly: `Remote::url()` applies `insteadOf` rewrites.
    let found = repo
        .config_snapshot()
        .string(&format!("remote.{REMOTE_NAME}.url"));
    let matches = found
        .as_ref()
        .and_then(|u| gix::url::parse(u.as_bstr()).ok())
        .is_some_and(|u| canonical(u) == canonical(expected));
    if !matches {
        return Err(SyncError::UrlMismatch {
            expected: url.to_owned(),
            found: found.map(|u| u.to_string()).unwrap_or_default(),
        });
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct RefInfo {
    pub name: String,
    /// Object the ref points to after peeling annotated tags.
    pub target: String,
}

#[derive(Debug, Serialize)]
pub struct Refs {
    pub head: Option<String>,
    pub branches: Vec<RefInfo>,
    pub tags: Vec<RefInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Submodule,
}

#[derive(Debug, Serialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: Kind,
    pub mode: String,
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: String,
}

#[derive(Debug, Serialize)]
pub struct CommitInfo {
    pub id: String,
    pub parents: Vec<String>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

pub fn list_refs(repo: &Repository) -> Result<Refs, AppError> {
    let head = repo
        .head_name()
        .map_err(AppError::internal)?
        .map(|n| n.shorten().to_string());
    let refs = repo.references().map_err(AppError::internal)?;
    let collect = |iter: gix::reference::iter::Iter<'_, '_>| -> Result<Vec<RefInfo>, AppError> {
        iter.map(|r| {
            let mut r = r.map_err(AppError::internal)?;
            let target = r.peel_to_id().map_err(AppError::internal)?.to_string();
            Ok(RefInfo {
                name: r.name().shorten().to_string(),
                target,
            })
        })
        .collect()
    };
    Ok(Refs {
        head,
        branches: collect(refs.local_branches().map_err(AppError::internal)?)?,
        tags: collect(refs.tags().map_err(AppError::internal)?)?,
    })
}

fn resolve_commit<'r>(repo: &'r Repository, rev: &str) -> Result<gix::Commit<'r>, AppError> {
    let not_found = || AppError::RefNotFound(rev.to_owned());
    repo.rev_parse_single(rev.as_bytes().as_bstr())
        .map_err(|_| not_found())?
        .object()
        .map_err(AppError::internal)?
        .peel_to_commit()
        .map_err(|_| not_found())
}

fn normalize(path: &str) -> &str {
    path.trim_matches('/')
}

fn entry_kind(kind: EntryKind) -> Kind {
    match kind {
        EntryKind::Tree => Kind::Dir,
        EntryKind::Blob | EntryKind::BlobExecutable => Kind::File,
        EntryKind::Link => Kind::Symlink,
        EntryKind::Commit => Kind::Submodule,
    }
}

/// Lists the directory at `path` (empty for the root) in the commit `rev` resolves to.
pub fn read_tree(repo: &Repository, rev: &str, path: &str) -> Result<Vec<TreeEntry>, AppError> {
    let path = normalize(path);
    let root = resolve_commit(repo, rev)?
        .tree()
        .map_err(AppError::internal)?;
    let tree = if path.is_empty() {
        root
    } else {
        let entry = root
            .lookup_entry_by_path(path)
            .map_err(AppError::internal)?
            .ok_or_else(|| AppError::PathNotFound(path.to_owned()))?;
        if !entry.mode().is_tree() {
            return Err(AppError::BadRequest(format!("not a directory: {path}")));
        }
        entry.object().map_err(AppError::internal)?.into_tree()
    };

    tree.iter()
        .map(|entry| {
            let entry = entry.map_err(AppError::internal)?;
            let kind = entry.mode().kind();
            let size = match kind {
                EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => Some(
                    repo.find_header(entry.oid())
                        .map_err(AppError::internal)?
                        .size(),
                ),
                EntryKind::Tree | EntryKind::Commit => None,
            };
            Ok(TreeEntry {
                name: entry.filename().to_str_lossy().into_owned(),
                kind: entry_kind(kind),
                mode: kind.as_octal_str().to_string(),
                oid: entry.oid().to_string(),
                size,
            })
        })
        .collect()
}

/// Returns the blob id of the file at `path`, so callers can answer conditional requests without reading it.
pub fn find_blob(repo: &Repository, rev: &str, path: &str) -> Result<ObjectId, AppError> {
    let path = normalize(path);
    let entry = resolve_commit(repo, rev)?
        .tree()
        .map_err(AppError::internal)?
        .lookup_entry_by_path(path)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::PathNotFound(path.to_owned()))?;
    if !entry.mode().is_blob_or_symlink() {
        return Err(AppError::BadRequest(format!("not a file: {path}")));
    }
    Ok(entry.object_id())
}

pub fn read_blob(repo: &Repository, id: ObjectId) -> Result<Vec<u8>, AppError> {
    Ok(repo
        .find_blob(id)
        .map_err(AppError::internal)?
        .detach()
        .data)
}

fn signature(sig: gix::actor::SignatureRef<'_>) -> Signature {
    Signature {
        name: sig.name.to_str_lossy().into_owned(),
        email: sig.email.to_str_lossy().into_owned(),
        time: sig.time().map_or_else(
            |_| sig.time.to_owned(),
            |t| t.format_or_unix(gix::date::time::format::ISO8601_STRICT),
        ),
    }
}

/// Walks history from `rev` newest first. With `path`, keeps only commits whose entry at `path`
/// differs from every parent's. Like `git log -- <path>`, a merge that took the entry unchanged
/// from one side is not listed.
pub fn log(
    repo: &Repository,
    rev: &str,
    path: Option<&str>,
    limit: usize,
) -> Result<Vec<CommitInfo>, AppError> {
    let start = resolve_commit(repo, rev)?.id;
    let path = path.map(normalize).filter(|p| !p.is_empty());
    let walk = repo
        .rev_walk([start])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            Default::default(),
        ))
        .all()
        .map_err(AppError::internal)?;

    // Every commit is visited once as itself and once as a parent, so memoize the path lookup.
    let mut entry_at: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
    let mut lookup = |id: ObjectId| -> Result<Option<ObjectId>, AppError> {
        let Some(path) = path else { return Ok(None) };
        if let Some(found) = entry_at.get(&id) {
            return Ok(*found);
        }
        let found = repo
            .find_commit(id)
            .map_err(AppError::internal)?
            .tree()
            .map_err(AppError::internal)?
            .lookup_entry_by_path(path)
            .map_err(AppError::internal)?
            .map(|e| e.object_id());
        entry_at.insert(id, found);
        Ok(found)
    };

    let mut commits = Vec::new();
    for info in walk {
        if commits.len() == limit {
            break;
        }
        let info = info.map_err(AppError::internal)?;
        if path.is_some() {
            let here = lookup(info.id)?;
            let mut unchanged = info.parent_ids.is_empty() && here.is_none();
            for p in &info.parent_ids {
                if lookup(*p)? == here {
                    unchanged = true;
                    break;
                }
            }
            if unchanged {
                continue;
            }
        }
        let commit = info.object().map_err(AppError::internal)?;
        commits.push(CommitInfo {
            id: info.id.to_string(),
            parents: info.parent_ids.iter().map(ToString::to_string).collect(),
            author: signature(commit.author().map_err(AppError::internal)?),
            committer: signature(commit.committer().map_err(AppError::internal)?),
            message: commit.message_raw_sloppy().to_str_lossy().into_owned(),
        });
    }
    Ok(commits)
}
