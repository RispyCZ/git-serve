use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    pub mirror: MirrorConfig,
    pub bind: SocketAddr,
}

pub struct MirrorConfig {
    /// Upstream repository. Must not carry a password: pass tokens in `token`.
    pub url: String,
    pub cache_dir: PathBuf,
    /// HTTP password for the upstream, handed to git through a credential helper.
    pub token: Option<String>,
    /// HTTP username sent with `token`.
    pub token_user: String,
    /// Requests within this long after the last sync attempt are answered from the cache
    /// without asking the upstream.
    pub ttl: Duration,
    /// Longest a single git process (clone, fetch, ...) may run before it is terminated.
    pub git_timeout: Duration,
}

impl MirrorConfig {
    pub fn new(url: impl Into<String>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            cache_dir: cache_dir.into(),
            token: None,
            // GitHub App installation tokens require this user; GitHub and GitLab personal
            // access tokens accept any.
            token_user: "x-access-token".to_owned(),
            ttl: Duration::from_secs(30),
            git_timeout: Duration::from_secs(600),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let url = std::env::var("GIT_SERVE_REPO_URL")
            .map_err(|_| "GIT_SERVE_REPO_URL must be set".to_owned())?;
        let cache_dir = std::env::var_os("GIT_SERVE_CACHE_DIR")
            .map_or_else(|| PathBuf::from("./cache"), PathBuf::from);
        let mut mirror = MirrorConfig::new(url, cache_dir);
        mirror.token = std::env::var("GIT_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        if let Ok(user) = std::env::var("GIT_SERVE_TOKEN_USER") {
            mirror.token_user = user;
        }
        if let Some(ttl) = seconds("GIT_SERVE_SYNC_TTL")? {
            mirror.ttl = ttl;
        }
        if let Some(timeout) = seconds("GIT_SERVE_GIT_TIMEOUT")? {
            mirror.git_timeout = timeout;
        }
        let bind = match std::env::var("GIT_SERVE_BIND") {
            Ok(s) => s
                .parse()
                .map_err(|e| format!("GIT_SERVE_BIND={s:?} is not a socket address: {e}"))?,
            Err(_) => SocketAddr::from(([0, 0, 0, 0], 8080)),
        };
        Ok(Self { mirror, bind })
    }
}

fn seconds(var: &str) -> Result<Option<Duration>, String> {
    std::env::var(var).ok().map_or(Ok(None), |s| {
        s.parse()
            .map(|n| Some(Duration::from_secs(n)))
            .map_err(|e| format!("{var}={s:?} is not a number of seconds: {e}"))
    })
}
