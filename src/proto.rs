//! The wire protocol between the daemon and its clients (TUI windows and the
//! `open` command), plus the filesystem locations they rendezvous on.
//!
//! Transport: a unix socket in a private directory. Each message is one line
//! of JSON. The client sends numbered [`ClientMsg`]s and the daemon answers
//! each with a [`ServerMsg::Reply`] carrying the same id; subscribed clients
//! also receive unsolicited [`ServerMsg::State`] and [`ServerMsg::Notice`]s.

use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

use crate::{
    actions::HookSpec,
    engine::{FileList, TorrentView},
    tags::Tags,
};

/// Bumped whenever the messages change incompatibly; checked on connect so a
/// freshly upgraded client doesn't talk garbage to a stale daemon.
pub const VERSION: u32 = 4;

/// Upper bound for one message. Nothing legitimate comes close.
const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientMsg {
    pub id: u64,
    pub req: Request,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Must be the first message. `subscribe` asks for live state updates
    /// (a UI does; `open` and `daemon stop` don't).
    Hello {
        version: u32,
        subscribe: bool,
    },
    /// Add a magnet link, URL, info-hash or absolute .torrent path, giving
    /// the torrent these tags once it exists.
    Add {
        input: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    TogglePause {
        id: usize,
    },
    Remove {
        id: usize,
        delete_files: bool,
    },
    /// The files of a torrent already in the session.
    Files {
        id: usize,
    },
    SetFiles {
        id: usize,
        only: Vec<usize>,
    },
    /// Answer a pending file selection (see [`State::pickers`]).
    ApplyPicker {
        token: u64,
        only: Vec<usize>,
    },
    CancelPicker {
        token: u64,
    },
    SetLimits {
        download: Option<u32>,
        upload: Option<u32>,
    },
    SetTags {
        info_hash: String,
        tags: Vec<String>,
    },
    RunActions {
        id: usize,
    },
    /// Replace the `on_complete` actions: saved to the config file and put
    /// into effect at once.
    SetHooks {
        hooks: Vec<HookSpec>,
    },
    /// Remove a tag from every torrent and from the actions' tag lists.
    DeleteTag {
        tag: String,
    },
    Status,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Reply {
        id: u64,
        result: Result<Reply, String>,
    },
    State(Box<State>),
    Notice {
        text: String,
        is_error: bool,
    },
    /// The daemon is shutting down.
    Closing,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Welcome(Welcome),
    /// Something to tell the user, e.g. "Limits applied but not saved: …".
    Message {
        text: String,
        is_error: bool,
    },
    Files {
        list: FileList,
        selected: Vec<usize>,
    },
    DaemonStatus(DaemonStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u32,
    pub pid: u32,
    pub download_dir: String,
    pub config_path: String,
    /// Whether multi-file torrents wait for a file selection.
    pub ask_files: bool,
    pub has_actions: bool,
    /// One-off startup message (e.g. a data migration), delivered once.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub torrents: usize,
    /// Attached UI windows.
    pub uis: usize,
    pub uptime_secs: u64,
}

/// A multi-file torrent whose metadata is resolved and which waits for the
/// user to choose files. Any attached window may answer; the first answer wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerInfo {
    pub token: u64,
    pub name: String,
    pub files: FileList,
}

/// Everything a UI needs to draw, pushed a couple of times a second.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub torrents: Vec<TorrentView>,
    pub tags: Tags,
    pub download_limit: Option<u32>,
    pub upload_limit: Option<u32>,
    pub pickers: Vec<PickerInfo>,
    /// Adds still resolving metadata.
    pub adding: usize,
    pub actions_running: usize,
    /// The configured `on_complete` actions.
    pub hooks: Vec<HookSpec>,
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut buf = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf).await?;
    w.flush().await
}

/// Read one message; `None` on a clean end of stream. Not cancel-safe: call
/// it from a dedicated task, never inside `select!`.
pub async fn read_frame<R, T>(r: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + AsyncRead + Unpin,
    T: DeserializeOwned,
{
    buf.clear();
    let n = (&mut *r)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        let why = if buf.len() > MAX_FRAME {
            "message too large"
        } else {
            "connection closed mid-message"
        };
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why));
    }
    buf.pop();
    serde_json::from_slice(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The private directory holding the socket and lock.
/// `$XDG_RUNTIME_DIR/torrentui`, else `~/.local/share/torrentui/run`.
/// Created with mode 0700 so other users can't reach the socket.
pub fn run_dir() -> Result<PathBuf> {
    let dir = match dirs::runtime_dir() {
        Some(d) => d.join("torrentui"),
        None => dirs::data_dir()
            .ok_or_else(|| eyre!("cannot determine a runtime or data directory"))?
            .join("torrentui")
            .join("run"),
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .wrap_err_with(|| format!("cannot create {}", dir.display()))?;
    // `recursive` doesn't touch an existing directory's mode.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .wrap_err_with(|| format!("cannot secure {}", dir.display()))?;
    Ok(dir)
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.sock"))
}

/// `sockaddr_un` holds ~108 bytes; stay safely under it.
const MAX_SOCKET_PATH: usize = 100;

/// A path that can be passed to `bind`/`connect`. When the real socket path is
/// too long, this is a short `/proc/self/fd/N/daemon.sock` that reaches the
/// same directory through an open descriptor (Linux), which the guard keeps
/// open.
pub struct Endpoint {
    path: PathBuf,
    _dir: Option<fs::File>,
}

impl Endpoint {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

pub fn endpoint() -> Result<Endpoint> {
    let real = socket_path()?;
    if real.as_os_str().len() <= MAX_SOCKET_PATH {
        return Ok(Endpoint {
            path: real,
            _dir: None,
        });
    }
    let dir = fs::File::open(run_dir()?)?;
    let via_proc = PathBuf::from(format!(
        "/proc/self/fd/{}/daemon.sock",
        std::os::fd::AsRawFd::as_raw_fd(&dir)
    ));
    if !std::path::Path::new("/proc/self/fd").is_dir() {
        return Err(eyre!(
            "the socket path {} is too long for a unix socket; set XDG_RUNTIME_DIR to a \
             shorter directory",
            real.display()
        ));
    }
    Ok(Endpoint {
        path: via_proc,
        _dir: Some(dir),
    })
}

pub fn lock_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("daemon.lock"))
}

/// Where the daemon's own stderr goes.
pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| eyre!("cannot determine your data directory"))?
        .join("torrentui")
        .join("daemon.log"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn frames_round_trip_and_end_cleanly() {
        let mut wire = Vec::new();
        for id in 0..3 {
            let msg = ClientMsg {
                id,
                req: Request::Add {
                    input: format!("magnet:?xt=urn:btih:{id}\nwith newline \"quotes\""),
                    tags: vec!["movies".into()],
                },
            };
            write_frame(&mut wire, &msg).await.unwrap();
        }
        // One line per message even though the payload contains a newline.
        assert_eq!(wire.iter().filter(|b| **b == b'\n').count(), 3);

        let mut r = BufReader::new(wire.as_slice());
        let mut buf = Vec::new();
        for id in 0..3 {
            let m: ClientMsg = read_frame(&mut r, &mut buf).await.unwrap().unwrap();
            assert_eq!(m.id, id);
            let Request::Add { input, .. } = m.req else {
                panic!()
            };
            assert!(input.contains("with newline \"quotes\""));
        }
        assert!(
            read_frame::<_, ClientMsg>(&mut r, &mut buf)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_garbage_truncated_and_oversized_frames() {
        let mut buf = Vec::new();
        let mut r = BufReader::new(&b"not json\n"[..]);
        let e = read_frame::<_, ClientMsg>(&mut r, &mut buf)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);

        let mut r = BufReader::new(&b"{\"id\":1,"[..]);
        let e = read_frame::<_, ClientMsg>(&mut r, &mut buf)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("mid-message"));

        let big = vec![b'x'; MAX_FRAME + 10];
        let mut r = BufReader::new(big.as_slice());
        let e = read_frame::<_, ClientMsg>(&mut r, &mut buf)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("too large"));
        assert!(buf.len() <= MAX_FRAME + 1, "reads stay bounded");
    }

    #[test]
    fn replies_and_state_serialise() {
        let msg = ServerMsg::Reply {
            id: 7,
            result: Err("nope".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMsg::Reply { id: 7, result: Err(e) } if e == "nope"));

        let state = State {
            torrents: vec![],
            tags: Tags::new(),
            download_limit: Some(5),
            upload_limit: None,
            pickers: vec![],
            adding: 1,
            actions_running: 2,
            hooks: vec![],
        };
        let json = serde_json::to_string(&ServerMsg::State(Box::new(state))).unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(&json).unwrap(),
            ServerMsg::State(s) if s.download_limit == Some(5) && s.actions_running == 2
        ));
    }

    #[test]
    fn non_utf8_output_folders_do_not_break_serialisation() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let view = TorrentView {
            id: 1,
            name: "n".into(),
            info_hash: "h".into(),
            output_folder: PathBuf::from(OsString::from_vec(b"/dl/bad\xff".to_vec())),
            status: crate::engine::Status::Seeding,
            progress_bytes: 0,
            total_bytes: 0,
            uploaded_bytes: 0,
            down_bps: 0,
            up_bps: 0,
            peers_live: 0,
            peers_connecting: 0,
            peers_queued: 0,
            peers_seen: 0,
            eta: None,
            error: None,
        };
        let json = serde_json::to_string(&view).unwrap();
        let back: TorrentView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.output_folder.to_string_lossy(), "/dl/bad\u{fffd}");
    }
}
