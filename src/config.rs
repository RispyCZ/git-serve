use std::net::SocketAddr;
use std::path::PathBuf;

pub struct Config {
    pub repo_url: String,
    pub cache_dir: PathBuf,
    pub bind: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let repo_url = std::env::var("GIT_SERVE_REPO_URL")
            .map_err(|_| "GIT_SERVE_REPO_URL must be set".to_owned())?;
        let cache_dir = std::env::var_os("GIT_SERVE_CACHE_DIR")
            .map_or_else(|| PathBuf::from("./cache"), PathBuf::from);
        let bind = match std::env::var("GIT_SERVE_BIND") {
            Ok(s) => s
                .parse()
                .map_err(|e| format!("GIT_SERVE_BIND={s:?} is not a socket address: {e}"))?,
            Err(_) => SocketAddr::from(([0, 0, 0, 0], 8080)),
        };
        Ok(Self {
            repo_url,
            cache_dir,
            bind,
        })
    }
}
