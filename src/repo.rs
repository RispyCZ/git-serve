use std::collections::HashMap;

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::{ObjectId, Repository};
use serde::Serialize;

use crate::error::AppError;

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

/// The commit `rev` names, peeling tags.
pub fn resolve_commit(repo: &Repository, rev: &str) -> Result<ObjectId, AppError> {
    let not_found = || AppError::RefNotFound(rev.to_owned());
    Ok(repo
        .rev_parse_single(rev.as_bytes().as_bstr())
        .map_err(|_| not_found())?
        .object()
        .map_err(AppError::internal)?
        .peel_to_commit()
        .map_err(|_| not_found())?
        .id)
}

fn root_tree(repo: &Repository, commit: ObjectId) -> Result<gix::Tree<'_>, AppError> {
    repo.find_commit(commit)
        .map_err(AppError::internal)?
        .tree()
        .map_err(AppError::internal)
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

/// Lists the directory at `path` (empty for the root) in `commit`.
pub fn read_tree(
    repo: &Repository,
    commit: ObjectId,
    path: &str,
) -> Result<Vec<TreeEntry>, AppError> {
    let path = normalize(path);
    let root = root_tree(repo, commit)?;
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

    // A partial clone may lack file contents, which hold the sizes: collect every missing one,
    // so they are fetched in one batch.
    let mut missing = Vec::new();
    let mut entries = Vec::new();
    for entry in tree.iter() {
        let entry = entry.map_err(AppError::internal)?;
        let kind = entry.mode().kind();
        let size = match kind {
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                let header = repo
                    .try_find_header(entry.oid())
                    .map_err(AppError::internal)?;
                if header.is_none() {
                    missing.push(entry.object_id());
                }
                header.map(|h| h.size())
            }
            EntryKind::Tree | EntryKind::Commit => None,
        };
        entries.push(TreeEntry {
            name: entry.filename().to_str_lossy().into_owned(),
            kind: entry_kind(kind),
            mode: kind.as_octal_str().to_string(),
            oid: entry.oid().to_string(),
            size,
        });
    }
    if !missing.is_empty() {
        return Err(AppError::MissingObjects(missing));
    }
    Ok(entries)
}

/// Returns the blob id of the file at `path`, so callers can answer conditional requests without reading it.
pub fn find_blob(repo: &Repository, commit: ObjectId, path: &str) -> Result<ObjectId, AppError> {
    let path = normalize(path);
    let entry = root_tree(repo, commit)?
        .lookup_entry_by_path(path)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::PathNotFound(path.to_owned()))?;
    if !entry.mode().is_blob_or_symlink() {
        return Err(AppError::BadRequest(format!("not a file: {path}")));
    }
    Ok(entry.object_id())
}

pub fn read_blob(repo: &Repository, id: ObjectId) -> Result<Vec<u8>, AppError> {
    let object = repo
        .try_find_object(id)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::MissingObjects(vec![id]))?;
    Ok(object
        .try_into_blob()
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

/// Walks history from `start` newest first. With `path`, keeps only commits whose entry at `path`
/// differs from every parent's. Like `git log -- <path>`, a merge that took the entry unchanged
/// from one side is not listed.
pub fn log(
    repo: &Repository,
    start: ObjectId,
    path: Option<&str>,
    limit: usize,
) -> Result<Vec<CommitInfo>, AppError> {
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
