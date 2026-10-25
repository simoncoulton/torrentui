//! Thin wrapper around the librqbit `Session`.
//!
//! The UI never touches librqbit types directly except through this module:
//! it asks for plain-data [`TorrentView`] snapshots and issues commands.

use std::{collections::HashSet, num::NonZeroU32, path::PathBuf, sync::Arc};

use color_eyre::eyre::{Result, WrapErr, eyre};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState, api::TorrentIdOrHash, limits::LimitsConfig,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Paths cross the daemon socket as text. A name that isn't valid UTF-8 would
/// make plain `PathBuf` serialisation fail for the whole state, so it is
/// converted lossily (the daemon keeps the exact value for its own use).
mod lossy_path {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&p.to_string_lossy())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(PathBuf::from(String::deserialize(d)?))
    }
}

/// Plain-data snapshot of one torrent, safe to hold across frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentView {
    pub id: usize,
    pub name: String,
    pub info_hash: String,
    #[serde(with = "lossy_path")]
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

/// Session-wide settings that come from the config file.
pub struct EngineOptions {
    pub download_dir: PathBuf,
    /// Where the torrent list and resume data are kept.
    pub sessions_dir: PathBuf,
    pub download_bps: Option<NonZeroU32>,
    pub upload_bps: Option<NonZeroU32>,
    /// Per-torrent peer cap; `None` uses the engine default.
    pub peer_limit: Option<usize>,
    pub dht: bool,
}

/// One selectable file inside a torrent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Index in the torrent's file list (what librqbit's `only_files` uses).
    pub index: usize,
    pub path: String,
    pub len: u64,
    /// Bytes already on disk; always 0 for a torrent that isn't added yet.
    pub done: u64,
}

/// The files of a torrent plus the BEP-47 padding files that must always be
/// kept in the selection (they are hidden from the user).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileList {
    pub entries: Vec<FileEntry>,
    pub padding: Vec<usize>,
}

impl FileList {
    /// Every file index, padding included.
    pub fn all_indices(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .entries
            .iter()
            .map(|e| e.index)
            .chain(self.padding.iter().copied())
            .collect();
        v.sort_unstable();
        v
    }
}

/// A torrent whose metadata is resolved but which has not been started yet.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub name: String,
    pub files: FileList,
    pub torrent_bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct Engine {
    session: Arc<Session>,
}

impl Engine {
    pub async fn start(opts: EngineOptions) -> Result<Self> {
        let download_dir = opts.download_dir;
        std::fs::create_dir_all(&download_dir)
            .wrap_err_with(|| format!("cannot create download dir {}", download_dir.display()))?;
        std::fs::create_dir_all(&opts.sessions_dir).wrap_err_with(|| {
            format!("cannot create sessions dir {}", opts.sessions_dir.display())
        })?;
        let session_opts = SessionOptions {
            // Remember torrents across restarts and resume them quickly.
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(opts.sessions_dir),
            }),
            dht: opts.dht.then(DhtSessionConfig::default),
            ratelimits: LimitsConfig {
                download_bps: opts.download_bps,
                upload_bps: opts.upload_bps,
            },
            peer_limit: opts.peer_limit,
            ..Default::default()
        };
        let session = Session::new_with_opts(download_dir, session_opts)
            .await
            .map_err(|e| eyre!("failed to start torrent session: {e:#}"))?;
        Ok(Self { session })
    }

    /// Change the global speed limits (bytes/s; `None` is unlimited) on the
    /// running session.
    pub fn set_limits(&self, download: Option<NonZeroU32>, upload: Option<NonZeroU32>) {
        self.session.ratelimits.set_download_bps(download);
        self.session.ratelimits.set_upload_bps(upload);
    }

    /// Turn user input (magnet, URL, info hash or local path) into something
    /// librqbit can add.
    async fn source(input: &str) -> Result<AddTorrent<'static>> {
        let input = normalize_input(input);
        if input.is_empty() {
            return Err(eyre!("nothing to add"));
        }
        if is_remote(&input) {
            Ok(AddTorrent::from_url(input))
        } else {
            let path = crate::config::expand_tilde(&input);
            let bytes = tokio::fs::read(&path)
                .await
                .wrap_err_with(|| format!("cannot read {}", path.display()))?;
            Ok(AddTorrent::from_bytes(bytes))
        }
    }

    fn add_options(only_files: Option<Vec<usize>>) -> AddTorrentOptions {
        AddTorrentOptions {
            // Required to resume a partially/fully downloaded torrent whose
            // files already exist. Pieces are hash-checked before being kept.
            overwrite: true,
            only_files,
            ..Default::default()
        }
    }

    async fn add_source(
        &self,
        add: AddTorrent<'_>,
        only_files: Option<Vec<usize>>,
    ) -> Result<String> {
        let resp = self
            .session
            .add_torrent(add, Some(Self::add_options(only_files)))
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

    /// Add a magnet link, `http(s)` URL to a .torrent, a bare 40-char info
    /// hash, or a path to a local .torrent file, downloading every file.
    /// Returns a display name.
    ///
    /// For magnet links this resolves metadata from the swarm first, so it can
    /// take a while; call it from a spawned task.
    pub async fn add(&self, input: &str) -> Result<String> {
        self.add_source(Self::source(input).await?, None).await
    }

    /// Resolve a torrent's metadata (waiting on the swarm for magnets) without
    /// starting it, so the user can choose files first.
    pub async fn prepare(&self, input: &str) -> Result<Prepared> {
        let opts = AddTorrentOptions {
            list_only: true,
            ..Default::default()
        };
        let resp = self
            .session
            .add_torrent(Self::source(input).await?, Some(opts))
            .await
            .map_err(|e| eyre!("{e:#}"))?;
        match resp {
            AddTorrentResponse::ListOnly(list) => {
                let mut files = FileList::default();
                for (index, d) in list.info.iter_file_details().enumerate() {
                    if d.attrs().padding {
                        files.padding.push(index);
                    } else {
                        files.entries.push(FileEntry {
                            index,
                            path: d.filename.to_string(),
                            len: d.len,
                            done: 0,
                        });
                    }
                }
                let name = list
                    .info
                    .name()
                    .map(|n| n.into_owned())
                    .unwrap_or_else(|| "torrent".into());
                Ok(Prepared {
                    name,
                    files,
                    torrent_bytes: list.torrent_bytes.to_vec(),
                })
            }
            AddTorrentResponse::AlreadyManaged(_, h) => Err(eyre!(
                "already added: {}",
                h.name().unwrap_or_else(|| "torrent".into())
            )),
            AddTorrentResponse::Added(..) => Err(eyre!("unexpected response while listing files")),
        }
    }

    /// Start a [`Prepared`] torrent, downloading only `only_files`.
    pub async fn add_prepared(
        &self,
        torrent_bytes: Vec<u8>,
        only_files: Vec<usize>,
    ) -> Result<String> {
        self.add_source(AddTorrent::from_bytes(torrent_bytes), Some(only_files))
            .await
    }

    /// Files of an existing torrent and which of them are currently selected.
    pub fn files(&self, id: usize) -> Result<(FileList, HashSet<usize>)> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or_else(|| eyre!("torrent {id} no longer exists"))?;
        let done = handle.stats().file_progress;
        let mut files = FileList::default();
        handle
            .with_metadata(|m| {
                for (index, f) in m.file_infos.iter().enumerate() {
                    if f.attrs.padding {
                        files.padding.push(index);
                    } else {
                        files.entries.push(FileEntry {
                            index,
                            path: f.relative_filename.display().to_string(),
                            len: f.len,
                            done: done.get(index).copied().unwrap_or(0),
                        });
                    }
                }
            })
            .map_err(|_| eyre!("metadata not resolved yet"))?;
        let selected = match handle.only_files() {
            Some(v) => v.into_iter().collect(),
            None => files.entries.iter().map(|e| e.index).collect(),
        };
        Ok((files, selected))
    }

    /// Where the downloaded data is: the file itself for a single-file
    /// torrent, otherwise the torrent's folder.
    pub fn content_path(&self, id: usize) -> Result<PathBuf> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or_else(|| eyre!("torrent {id} no longer exists"))?;
        let folder = handle.output_folder().to_path_buf();
        let single = handle
            .with_metadata(|m| {
                let mut real = m.file_infos.iter().filter(|f| !f.attrs.padding);
                match (real.next(), real.next()) {
                    (Some(only), None) => Some(only.relative_filename.clone()),
                    _ => None,
                }
            })
            .map_err(|_| eyre!("metadata not resolved yet"))?;
        Ok(match single {
            Some(file) => folder.join(file),
            None => folder,
        })
    }

    /// Change which files an existing torrent downloads.
    pub async fn set_files(&self, id: usize, only_files: Vec<usize>) -> Result<()> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or_else(|| eyre!("torrent {id} no longer exists"))?;
        let set: HashSet<usize> = only_files.into_iter().collect();
        self.session
            .update_only_files(&handle, &set)
            .await
            .map_err(|e| eyre!("{e:#}"))
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

/// Where the engine itself keeps session data by default; used only to
/// migrate older installs to our own directory.
pub fn legacy_sessions_dir() -> Option<PathBuf> {
    SessionPersistenceConfig::default_json_persistence_folder().ok()
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

/// Turn what the user typed or the browser passed into something the daemon
/// can act on from its own working directory: quotes are stripped, `file://`
/// URLs become paths, and local paths become absolute (the daemon was not
/// started from where the user is standing).
pub fn prepare_input(raw: &str) -> String {
    let input = normalize_input(raw);
    if input.is_empty() {
        return input;
    }
    if let Some(path) = file_url_to_path(&input) {
        return absolute(&path);
    }
    if is_remote(&input) {
        return input;
    }
    absolute(&input)
}

fn absolute(path: &str) -> String {
    let expanded = crate::config::expand_tilde(path);
    std::path::absolute(&expanded)
        .unwrap_or(expanded)
        .to_string_lossy()
        .into_owned()
}

/// `file:///home/me/a%20b.torrent` -> `/home/me/a b.torrent`.
fn file_url_to_path(input: &str) -> Option<String> {
    let rest = input.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn is_remote(input: &str) -> bool {
    librqbit::SUPPORTED_SCHEMES
        .iter()
        .any(|scheme| input.starts_with(scheme))
        || (input.len() == 40
            && input.chars().all(|c| c.is_ascii_hexdigit())
            && !std::path::Path::new(input).exists())
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

    #[test]
    fn file_urls_become_paths() {
        assert_eq!(
            file_url_to_path("file:///home/me/a%20b%23.torrent").as_deref(),
            Some("/home/me/a b#.torrent")
        );
        assert_eq!(
            file_url_to_path("file://localhost/x").as_deref(),
            Some("/x")
        );
        assert_eq!(file_url_to_path("file://host/x"), None);
        assert_eq!(file_url_to_path("https://x/y"), None);
        // Bad escapes are kept literally rather than panicking.
        assert_eq!(
            file_url_to_path("file:///a%zz%4").as_deref(),
            Some("/a%zz%4")
        );
    }

    #[test]
    fn prepare_input_makes_local_paths_absolute_and_leaves_remote_alone() {
        assert_eq!(
            prepare_input("magnet:?xt=urn:btih:abc"),
            "magnet:?xt=urn:btih:abc"
        );
        assert_eq!(
            prepare_input("  \"https://x/a.torrent\" "),
            "https://x/a.torrent"
        );
        assert_eq!(
            prepare_input("file:///tmp/a%20b.torrent"),
            "/tmp/a b.torrent"
        );
        assert_eq!(prepare_input("/tmp/x.torrent"), "/tmp/x.torrent");
        let rel = prepare_input("some/where.torrent");
        assert!(std::path::Path::new(&rel).is_absolute() && rel.ends_with("some/where.torrent"));
        assert_eq!(prepare_input("   "), "");
    }
}
