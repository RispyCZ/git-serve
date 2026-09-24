//! Keeps the bare mirror in the cache dir in step with the upstream repository.
//!
//! A single background worker runs every git command, so concurrent requests share one sync.
//! Requests call [`Mirror::ensure_fresh`]: within the TTL of the last attempt they are answered
//! from the cache, otherwise they wait for a sync. A failed sync leaves the cache as it was,
//! and it is served stale until a later attempt succeeds.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use gix::bstr::ByteSlice;
use gix::{ObjectId, ThreadSafeRepository};
use nix::sys::signal::{Signal, killpg};
use serde::{Serialize, Serializer};
use tokio::process::Command;
use tokio::sync::{Notify, watch};

use crate::config::MirrorConfig;

/// The cache is a bare mirror: remote branches and tags map 1:1 onto local refs.
const REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];
const REMOTE_NAME: &str = "origin";

/// Pinned so the host's gitconfig cannot change them:
/// - protocol v2 filters the ref advertisement by refspec, so a fetch that finds nothing new
///   costs one round trip;
/// - no automatic gc or maintenance, which would repack while requests read the packs;
/// - commit-graph and reverse index files speed up history walks and object lookups.
const GIT_CONFIG: [&str; 5] = [
    "protocol.version=2",
    "gc.auto=0",
    "maintenance.auto=false",
    "fetch.writeCommitGraph=true",
    "pack.writeReverseIndex=true",
];

/// Answers credential requests from the environment, so the token is never in argv or on disk.
const CREDENTIAL_HELPER: &str = r#"!f() { test "$1" = get && printf 'username=%s\npassword=%s\n' "$GIT_SERVE_TOKEN_USER" "$GIT_SERVE_TOKEN"; }; f"#;

/// How long a git process gets to remove its lockfiles and temporary files after SIGTERM.
const TERM_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("repository URL must not contain a password; pass the token in GIT_SERVE_TOKEN")]
    PasswordInUrl,
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

/// Outcome of the most recent sync attempt.
#[derive(Clone, Default, Serialize)]
pub struct Status {
    /// Ticket of the last finished sync; see [`Schedule`].
    #[serde(skip)]
    completed: u64,
    #[serde(rename = "last_sync_at", serialize_with = "iso8601")]
    pub last_attempt: Option<SystemTime>,
    #[serde(rename = "last_success_at", serialize_with = "iso8601")]
    pub last_success: Option<SystemTime>,
    pub error: Option<String>,
}

impl Status {
    fn is_fresh(&self, ttl: Duration) -> bool {
        // A clock that went backwards reads as stale.
        self.last_attempt
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < ttl)
    }
}

fn iso8601<S: Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
    let formatted = t.map(|t| {
        let secs = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        gix::date::Time::new(i64::try_from(secs).unwrap_or(i64::MAX), 0)
            .format_or_unix(gix::date::time::format::ISO8601_STRICT)
    });
    formatted.serialize(s)
}

/// Syncs are numbered. A caller takes a ticket and waits until `Status::completed` reaches it.
struct Schedule {
    /// Highest ticket handed out.
    requested: u64,
    /// Ticket the worker took most recently; the sync for it is running unless it has completed.
    started: u64,
}

struct Credentials {
    /// Config key that scopes the helper to the upstream host, so a redirect elsewhere gets nothing.
    helper_key: String,
    user: String,
    token: String,
}

pub struct Mirror {
    url: String,
    dir: PathBuf,
    credentials: Option<Credentials>,
    ttl: Duration,
    git_timeout: Duration,
    blobless: bool,
    /// Serializes object fetches, so concurrent requests for the same files download them once.
    object_fetch: tokio::sync::Mutex<()>,
    /// Set once the cache holds a repository: at startup, or by the worker after cloning.
    repo: OnceLock<ThreadSafeRepository>,
    schedule: Mutex<Schedule>,
    wake: Notify,
    status: watch::Sender<Status>,
    shutdown: watch::Sender<bool>,
}

impl Mirror {
    /// Checks the cache dir without touching the network: it must be missing, empty, or a
    /// mirror of `config.url`. The first sync is queued; [`Mirror::spawn`] runs it.
    pub fn open(config: MirrorConfig) -> Result<Self, SyncError> {
        let url = gix::url::parse(config.url.as_bytes().as_bstr()).map_err(SyncError::git)?;
        if url.password().is_some() {
            return Err(SyncError::PasswordInUrl);
        }
        let credentials = config.token.map(|token| Credentials {
            helper_key: format!("credential.{}.helper", origin_of(&url)),
            user: config.token_user,
            token,
        });

        let dir = config.cache_dir;
        let repo = OnceLock::new();
        if !is_empty_dir(&dir)? {
            let existing = gix::open(&dir).map_err(|source| SyncError::NotARepo {
                path: dir.display().to_string(),
                source: Box::new(source),
            })?;
            check_origin(&existing, &config.url)?;
            let _ = repo.set(existing.into_sync());
        }

        Ok(Self {
            url: config.url,
            dir,
            credentials,
            ttl: config.ttl,
            git_timeout: config.git_timeout,
            blobless: config.blobless,
            object_fetch: tokio::sync::Mutex::new(()),
            repo,
            schedule: Mutex::new(Schedule {
                requested: 1,
                started: 0,
            }),
            wake: Notify::new(),
            status: watch::Sender::new(Status::default()),
            shutdown: watch::Sender::new(false),
        })
    }

    /// Starts the worker that runs syncs. It returns after [`Mirror::shutdown`].
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(Arc::clone(self).work())
    }

    /// The repository, once the cache holds one.
    pub fn repo(&self) -> Option<&ThreadSafeRepository> {
        self.repo.get()
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// Waits for a sync unless the last attempt is within the TTL. With `force`, always waits for
    /// a sync that starts after this call, so a push the caller was notified about is picked up.
    pub async fn ensure_fresh(&self, force: bool) -> Status {
        let mut rx = self.status.subscribe();
        let ticket = {
            let mut schedule = self.schedule();
            let status = rx.borrow();
            let queued = schedule.requested > schedule.started;
            let running = schedule.started > status.completed;
            if !force && status.is_fresh(self.ttl) {
                return status.clone();
            }
            if queued || (running && !force) {
                schedule.requested
            } else {
                schedule.requested += 1;
                self.wake.notify_one();
                schedule.requested
            }
        };
        tokio::select! {
            _ = rx.wait_for(|s| s.completed >= ticket) => {}
            () = self.shut_down() => {}
        }
        self.status()
    }

    /// Stops the worker. A running git process gets SIGTERM so it cleans up after itself.
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    async fn shut_down(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|stop| *stop).await;
    }

    fn schedule(&self) -> MutexGuard<'_, Schedule> {
        self.schedule.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn work(self: Arc<Self>) {
        // Whether upstream HEAD was compared since startup; see `update_head`.
        let mut head_checked = false;
        loop {
            let ticket = loop {
                {
                    let mut schedule = self.schedule();
                    if schedule.requested > schedule.started {
                        schedule.started = schedule.requested;
                        break schedule.started;
                    }
                }
                tokio::select! {
                    () = self.wake.notified() => {}
                    () = self.shut_down() => return,
                }
            };

            let result = self.sync(&mut head_checked).await;
            let now = SystemTime::now();
            self.status.send_modify(|status| {
                status.completed = ticket;
                status.last_attempt = Some(now);
                match result {
                    Ok(()) => {
                        status.last_success = Some(now);
                        status.error = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sync failed, serving cached repository");
                        status.error = Some(e.to_string());
                    }
                }
            });
            if *self.shutdown.borrow() {
                return;
            }
        }
    }

    async fn sync(self: &Arc<Self>, head_checked: &mut bool) -> Result<(), SyncError> {
        if self.repo.get().is_none() {
            self.clone_repo().await?;
            // Clone sets HEAD from the upstream.
            *head_checked = true;
            return Ok(());
        }

        let before = self.ref_state().await?;
        tracing::debug!(url = %self.url, "fetching");
        self.run(
            "fetch",
            self.git_dir()
                .args(["fetch", "--prune", "--quiet", "--no-write-fetch-head"])
                .arg(REMOTE_NAME)
                .args(REFSPECS),
        )
        .await?;
        let changed = self.ref_state().await? != before;
        if changed {
            tracing::info!(url = %self.url, "fetched new refs");
        }
        if changed || !*head_checked {
            match self.update_head().await {
                Ok(()) => *head_checked = true,
                Err(e) => tracing::warn!(error = %e, "cannot update HEAD from upstream"),
            }
        }
        Ok(())
    }

    async fn clone_repo(self: &Arc<Self>) -> Result<(), SyncError> {
        // `open` found the dir missing or empty, so anything in it now is left over from a
        // clone of ours that was killed.
        clear_dir(&self.dir)?;
        tracing::info!(url = %self.url, dir = %self.dir.display(), blobless = self.blobless, "cloning");
        let mut clone = self.git();
        clone.args(["clone", "--bare", "--quiet", "--origin", REMOTE_NAME]);
        if self.blobless {
            // Records the upstream as a promisor remote, so later fetches keep the filter.
            clone.arg("--filter=blob:none");
        }
        self.run("clone", clone.arg("--").arg(&self.url).arg(&self.dir))
            .await?;
        // `fetch.writeCommitGraph` covers later fetches but not the clone.
        if let Err(e) = self
            .run(
                "commit-graph",
                self.git_dir()
                    .args(["commit-graph", "write", "--reachable"]),
            )
            .await
        {
            tracing::warn!(error = %e, "cannot write commit-graph");
        }
        let repo = gix::open(&self.dir).map_err(SyncError::git)?;
        let _ = self.repo.set(repo.into_sync());
        Ok(())
    }

    /// `git fetch` never moves HEAD of a bare repository, so follow the upstream's default branch
    /// here. `ls-remote` sends no ref prefix for a `HEAD` pattern and lists every upstream ref,
    /// so this runs only after refs changed and once per process start.
    async fn update_head(self: &Arc<Self>) -> Result<(), SyncError> {
        let out = self
            .run(
                "ls-remote",
                self.git_dir()
                    .args(["ls-remote", "--symref", REMOTE_NAME, "HEAD"]),
            )
            .await?;
        // An upstream with a detached HEAD has no `ref:` line; keep ours.
        let Some(target) = out.lines().find_map(|l| {
            l.strip_prefix("ref: ")?
                .strip_suffix("\tHEAD")
                .filter(|t| t.starts_with("refs/heads/"))
        }) else {
            return Ok(());
        };
        let target = target.to_owned();
        let needs_update = self
            .with_repo({
                let target = target.clone();
                move |repo| {
                    let current = repo.head_name().map_err(SyncError::git)?;
                    let exists = repo
                        .try_find_reference(target.as_str())
                        .map_err(SyncError::git)?
                        .is_some();
                    Ok(exists && current.is_none_or(|c| c.as_bstr() != target.as_bytes()))
                }
            })
            .await?;
        if needs_update {
            tracing::info!(head = %target, "upstream default branch changed");
            self.run(
                "symbolic-ref",
                self.git_dir().args(["symbolic-ref", "HEAD", &target]),
            )
            .await?;
        }
        Ok(())
    }

    /// Fetches objects a partial clone does not hold yet, such as file contents in blobless mode.
    pub async fn fetch_objects(self: &Arc<Self>, ids: Vec<ObjectId>) -> Result<(), SyncError> {
        let _guard = self.object_fetch.lock().await;
        // Another request may have fetched them while this one waited.
        let missing = self
            .with_repo(move |repo| {
                Ok(ids
                    .into_iter()
                    .filter(|id| !repo.has_object(id))
                    .collect::<Vec<_>>())
            })
            .await?;
        if missing.is_empty() {
            return Ok(());
        }
        tracing::debug!(count = missing.len(), "fetching objects");
        let input: String = missing.iter().map(|id| format!("{id}\n")).collect();
        self.run_with_input(
            "fetch <objects>",
            self.git_dir()
                .args([
                    // Asking for known objects needs no negotiation, and they add no commits.
                    "-c",
                    "fetch.negotiationAlgorithm=noop",
                    "-c",
                    "fetch.writeCommitGraph=false",
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--recurse-submodules=no",
                    "--stdin",
                ])
                .arg(REMOTE_NAME),
            Some(input.into_bytes()),
        )
        .await?;
        Ok(())
    }

    /// Every ref and its target, to tell whether a fetch changed anything.
    async fn ref_state(self: &Arc<Self>) -> Result<Vec<(String, Option<ObjectId>)>, SyncError> {
        self.with_repo(|repo| {
            repo.references()
                .map_err(SyncError::git)?
                .all()
                .map_err(SyncError::git)?
                .map(|r| {
                    let r = r.map_err(SyncError::git)?;
                    Ok((
                        r.name().as_bstr().to_string(),
                        r.target().try_id().map(ToOwned::to_owned),
                    ))
                })
                .collect()
        })
        .await
    }

    async fn with_repo<T, F>(self: &Arc<Self>, f: F) -> Result<T, SyncError>
    where
        T: Send + 'static,
        F: FnOnce(&gix::Repository) -> Result<T, SyncError> + Send + 'static,
    {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let repo = this
                .repo
                .get()
                .ok_or_else(|| SyncError::Git("cache holds no repository".to_owned()))?;
            f(&repo.to_thread_local())
        })
        .await
        .map_err(SyncError::git)?
    }

    /// A git command with the pinned config and, when configured, the upstream credentials.
    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        for kv in GIT_CONFIG {
            cmd.arg("-c").arg(kv);
        }
        if let Some(c) = &self.credentials {
            // The empty value drops helpers from the host's gitconfig for this host.
            cmd.arg("-c")
                .arg(format!("{}=", c.helper_key))
                .arg("-c")
                .arg(format!("{}={CREDENTIAL_HELPER}", c.helper_key))
                .env("GIT_SERVE_TOKEN_USER", &c.user)
                .env("GIT_SERVE_TOKEN", &c.token);
        }
        // Missing credentials must fail instead of waiting for a prompt.
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so `run` can signal git together with its transport helpers.
            .process_group(0)
            .kill_on_drop(true);
        cmd
    }

    fn git_dir(&self) -> Command {
        let mut cmd = self.git();
        cmd.arg("--git-dir").arg(&self.dir);
        cmd
    }

    /// Runs `cmd` to completion and returns its stdout. On timeout or shutdown its process group
    /// gets SIGTERM, so git removes its lockfiles, and SIGKILL if still running after `TERM_GRACE`.
    async fn run(&self, what: &str, cmd: &mut Command) -> Result<String, SyncError> {
        self.run_with_input(what, cmd, None).await
    }

    /// [`Mirror::run`] with `input` written to the process's stdin.
    async fn run_with_input(
        &self,
        what: &str,
        cmd: &mut Command,
        input: Option<Vec<u8>>,
    ) -> Result<String, SyncError> {
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| SyncError::Git(format!("cannot run git: {e}")))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // Written concurrently with reading the output; closing stdin ends git's input.
            tokio::spawn(async move {
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stdin, &input).await;
            });
        }
        let pid = child.id();
        let output = child.wait_with_output();
        tokio::pin!(output);
        let reason = tokio::select! {
            out = &mut output => {
                let out = out.map_err(|e| SyncError::Git(format!("git {what}: {e}")))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(SyncError::Git(format!("git {what}: {}", stderr.trim())));
                }
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            () = tokio::time::sleep(self.git_timeout) => "timed out",
            () = self.shut_down() => "interrupted by shutdown",
        };
        // Git runs transports as child processes (`git remote-https`), which would be orphaned and
        // keep hanging on the upstream if only git itself were signalled.
        let group = pid
            .and_then(|p| i32::try_from(p).ok())
            .map(nix::unistd::Pid::from_raw);
        if let Some(group) = group {
            let _ = killpg(group, Signal::SIGTERM);
        }
        if tokio::time::timeout(TERM_GRACE, &mut output).await.is_err()
            && let Some(group) = group
        {
            let _ = killpg(group, Signal::SIGKILL);
        }
        Err(SyncError::Git(format!("git {what} {reason}")))
    }
}

/// `scheme://host[:port]`, the part of `url` credentials are scoped to.
fn origin_of(url: &gix::Url) -> String {
    let host = url.host().unwrap_or_default();
    match url.port {
        Some(port) => format!("{}://{host}:{port}", url.scheme.as_str()),
        None => format!("{}://{host}", url.scheme.as_str()),
    }
}

fn is_empty_dir(dir: &Path) -> Result<bool, SyncError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(SyncError::Io {
            path: dir.display().to_string(),
            source,
        }),
    }
}

/// Removes everything inside `dir`, keeping `dir` itself, which may be a mount point.
fn clear_dir(dir: &Path) -> Result<(), SyncError> {
    let io = |source| SyncError::Io {
        path: dir.display().to_string(),
        source,
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io)?;
        let path = entry.path();
        if entry.file_type().map_err(io)?.is_dir() {
            std::fs::remove_dir_all(&path).map_err(io)?;
        } else {
            std::fs::remove_file(&path).map_err(io)?;
        }
    }
    Ok(())
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

/// `url` with any password removed, for error messages.
fn redacted(url: &str) -> String {
    match gix::url::parse(url.as_bytes().as_bstr()) {
        Ok(mut u) if u.password().is_some() => {
            u.set_password(Some("***".to_owned()));
            u.to_bstring().to_string()
        }
        _ => url.to_owned(),
    }
}

fn check_origin(repo: &gix::Repository, url: &str) -> Result<(), SyncError> {
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
            found: found.map(|u| redacted(&u.to_string())).unwrap_or_default(),
        });
    }
    Ok(())
}
