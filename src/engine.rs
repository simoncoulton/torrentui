//! Thin wrapper around the librqbit `Session`.
//!
//! The UI never touches librqbit types directly except through this module:
//! it asks for plain-data [`TorrentView`] snapshots and issues commands.

use std::{path::PathBuf, sync::Arc};

use color_eyre::eyre::{Result, WrapErr, eyre};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState, api::TorrentIdOrHash,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Initializing,
    Downloading,
    Seeding,
    Paused,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Initializing => "Checking",
            Status::Downloading => "Downloading",
            Status::Seeding => "Seeding",
            Status::Paused => "Paused",
            Status::Error => "Error",
        }
    }
}

/// Plain-data snapshot of one torrent, safe to hold across frames.
#[derive(Debug, Clone)]
pub struct TorrentView {
    pub id: usize,
    pub name: String,
    pub info_hash: String,
    pub output_folder: PathBuf,
    pub status: Status,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub peers_live: u32,
    pub peers_connecting: u32,
    pub peers_queued: u32,
    pub peers_seen: u32,
    pub eta: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Engine {
    session: Arc<Session>,
}

impl Engine {
    pub async fn start(download_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&download_dir)
            .wrap_err_with(|| format!("cannot create download dir {}", download_dir.display()))?;
        let opts = SessionOptions {
            // Remember torrents across restarts and resume them quickly.
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json { folder: None }),
            ..Default::default()
        };
        let session = Session::new_with_opts(download_dir, opts)
            .await
            .map_err(|e| eyre!("failed to start torrent session: {e:#}"))?;
        Ok(Self { session })
    }

    /// Add a magnet link, `http(s)` URL to a .torrent, a bare 40-char info
    /// hash, or a path to a local .torrent file. Returns a display name.
    ///
    /// For magnet links this resolves metadata from the swarm first, so it can
    /// take a while; call it from a spawned task.
    pub async fn add(&self, input: &str) -> Result<String> {
        let input = normalize_input(input);
        if input.is_empty() {
            return Err(eyre!("nothing to add"));
        }
        let add = if is_remote(&input) {
            AddTorrent::from_url(input.clone())
        } else {
            let path = expand_tilde(&input);
            let bytes = tokio::fs::read(&path)
                .await
                .wrap_err_with(|| format!("cannot read {}", path.display()))?;
            AddTorrent::from_bytes(bytes)
        };
        let opts = AddTorrentOptions {
            // Required to resume a partially/fully downloaded torrent whose
            // files already exist. Pieces are hash-checked before being kept.
            overwrite: true,
            ..Default::default()
        };
        let resp = self
            .session
            .add_torrent(add, Some(opts))
            .await
            .map_err(|e| eyre!("{e:#}"))?;
        match resp {
            AddTorrentResponse::Added(_, h) => Ok(h.name().unwrap_or_else(|| "torrent".into())),
            AddTorrentResponse::AlreadyManaged(_, h) => Err(eyre!(
                "already added: {}",
                h.name().unwrap_or_else(|| "torrent".into())
            )),
            AddTorrentResponse::ListOnly(_) => Err(eyre!("unexpected list-only response")),
        }
    }

    /// Snapshot every torrent, ordered by id.
    pub fn snapshot(&self) -> Vec<TorrentView> {
        let mut views: Vec<TorrentView> = self.session.with_torrents(|it| {
            it.map(|(id, h)| {
                let stats = h.stats();
                // peers = [live, connecting, queued, seen]
                let (down_bps, up_bps, peers, eta) = match &stats.live {
                    Some(live) => (
                        live.download_speed.as_bytes(),
                        live.upload_speed.as_bytes(),
                        [
                            live.snapshot.peer_stats.live,
                            live.snapshot.peer_stats.connecting,
                            live.snapshot.peer_stats.queued,
                            live.snapshot.peer_stats.seen,
                        ],
                        live.time_remaining.as_ref().map(|d| d.to_string()),
                    ),
                    None => (0, 0, [0; 4], None),
                };
                let status = match stats.state {
                    TorrentStatsState::Initializing { .. } => Status::Initializing,
                    TorrentStatsState::Paused => Status::Paused,
                    TorrentStatsState::Error => Status::Error,
                    TorrentStatsState::Live if stats.finished => Status::Seeding,
                    TorrentStatsState::Live => Status::Downloading,
                };
                TorrentView {
                    id,
                    name: h.name().unwrap_or_else(|| "resolving metadata…".into()),
                    info_hash: h.info_hash().as_string(),
                    output_folder: h.output_folder().to_path_buf(),
                    status,
                    progress_bytes: stats.progress_bytes,
                    total_bytes: stats.total_bytes,
                    uploaded_bytes: stats.uploaded_bytes,
                    down_bps,
                    up_bps,
                    peers_live: peers[0],
                    peers_connecting: peers[1],
                    peers_queued: peers[2],
                    peers_seen: peers[3],
                    eta,
                    error: stats.error,
                }
            })
            .collect()
        });
        views.sort_by_key(|v| v.id);
        views
    }

    pub async fn toggle_pause(&self, id: usize) -> Result<()> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or_else(|| eyre!("torrent {id} no longer exists"))?;
        if handle.is_paused() {
            self.session
                .unpause(&handle)
                .await
                .map_err(|e| eyre!("{e:#}"))
        } else {
            self.session
                .pause(&handle)
                .await
                .map_err(|e| eyre!("{e:#}"))
        }
    }

    pub async fn remove(&self, id: usize, delete_files: bool) -> Result<()> {
        self.session
            .delete(TorrentIdOrHash::Id(id), delete_files)
            .await
            .map_err(|e| eyre!("{e:#}"))
    }

    pub async fn shutdown(&self) {
        self.session.stop().await;
    }
}

/// Strip whitespace and the quotes terminals add when dragging/pasting paths.
pub fn normalize_input(raw: &str) -> String {
    let s = raw.trim();
    let s = s
        .strip_prefix('\'')
        .and_then(|x| x.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|x| x.strip_suffix('"')))
        .unwrap_or(s);
    s.trim().to_string()
}

fn is_remote(input: &str) -> bool {
    librqbit::SUPPORTED_SCHEMES
        .iter()
        .any(|scheme| input.starts_with(scheme))
        || (input.len() == 40
            && input.chars().all(|c| c.is_ascii_hexdigit())
            && !std::path::Path::new(input).exists())
}

fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_quotes_and_space() {
        assert_eq!(normalize_input("  'a b.torrent'  "), "a b.torrent");
        assert_eq!(normalize_input("\"x\""), "x");
        assert_eq!(normalize_input("magnet:?xt=1"), "magnet:?xt=1");
    }

    #[test]
    fn detects_remote() {
        assert!(is_remote("magnet:?xt=urn:btih:abc"));
        assert!(is_remote("https://example.com/a.torrent"));
        assert!(is_remote(&"a".repeat(40)));
        assert!(!is_remote("/tmp/a.torrent"));
        assert!(!is_remote("relative.torrent"));
    }
}
