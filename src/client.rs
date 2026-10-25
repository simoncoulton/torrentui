//! Talking to the daemon: connecting (starting it if needed), request/reply
//! correlation, and the stream of pushed state and notices.

use std::{
    collections::HashMap,
    fs::{self, File},
    os::unix::process::CommandExt,
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use tokio::{
    io::BufReader,
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{mpsc, oneshot},
};

use crate::{
    daemon,
    proto::{
        self, ClientMsg, DaemonStatus, Reply, Request, ServerMsg, State, Welcome, read_frame,
        write_frame,
    },
};

/// Something the daemon pushed without being asked.
#[derive(Debug)]
pub enum Event {
    State(Box<State>),
    Notice {
        text: String,
        is_error: bool,
    },
    /// The connection ended; the reason is for the user.
    Closed(String),
}

type Pending = Option<HashMap<u64, oneshot::Sender<Result<Reply, String>>>>;

struct Link {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    /// `None` once the connection has closed.
    pending: Mutex<Pending>,
    next_id: AtomicU64,
}

#[derive(Clone)]
pub struct Client {
    link: Arc<Link>,
}

pub struct Connection {
    pub client: Client,
    pub welcome: Welcome,
    pub events: mpsc::UnboundedReceiver<Event>,
}

const LOST: &str = "the connection to the daemon was lost";

impl Client {
    pub async fn request(&self, req: Request) -> Result<Reply> {
        let id = self.link.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.link.pending.lock().unwrap_or_else(|e| e.into_inner());
            match pending.as_mut() {
                Some(map) => map.insert(id, tx),
                None => bail!(LOST),
            };
        }
        let sent = write_frame(&mut *self.link.writer.lock().await, &ClientMsg { id, req }).await;
        if sent.is_err() {
            if let Some(map) = self
                .link
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
                map.remove(&id);
            }
            bail!(LOST);
        }
        match rx.await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(msg)) => Err(eyre!(msg)),
            Err(_) => bail!(LOST),
        }
    }

    /// Like [`request`](Self::request) for calls that answer with a message
    /// for the status bar (or nothing).
    pub async fn request_message(&self, req: Request) -> Result<Option<(String, bool)>> {
        Ok(match self.request(req).await? {
            Reply::Message { text, is_error } => Some((text, is_error)),
            _ => None,
        })
    }

    pub async fn status(&self) -> Result<DaemonStatus> {
        match self.request(Request::Status).await? {
            Reply::DaemonStatus(s) => Ok(s),
            other => bail!("unexpected reply: {other:?}"),
        }
    }
}

fn not_running(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Connect and greet the daemon. `None` means no daemon is listening.
pub async fn connect(subscribe: bool) -> Result<Option<Connection>> {
    let endpoint = proto::endpoint()?;
    let path = proto::socket_path()?;
    let stream = match UnixStream::connect(endpoint.path()).await {
        Ok(s) => s,
        Err(e) if not_running(&e) => return Ok(None),
        Err(e) => return Err(e).wrap_err_with(|| format!("cannot connect to {}", path.display())),
    };
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let mut buf = Vec::new();

    let hello = ClientMsg {
        id: 0,
        req: Request::Hello {
            version: proto::VERSION,
            subscribe,
        },
    };
    write_frame(&mut wr, &hello).await?;
    let first = tokio::time::timeout(
        Duration::from_secs(15),
        read_frame::<_, ServerMsg>(&mut rd, &mut buf),
    )
    .await
    .map_err(|_| eyre!("the daemon did not answer"))??;
    let welcome = match first {
        Some(ServerMsg::Reply {
            result: Ok(Reply::Welcome(w)),
            ..
        }) => w,
        Some(ServerMsg::Reply {
            result: Err(msg), ..
        }) => bail!("{msg}"),
        _ => bail!("unexpected greeting from the daemon"),
    };

    let link = Arc::new(Link {
        writer: tokio::sync::Mutex::new(wr),
        pending: Mutex::new(Some(HashMap::new())),
        next_id: AtomicU64::new(1),
    });
    let (events_tx, events) = mpsc::unbounded_channel();
    let reader_link = link.clone();
    tokio::spawn(async move {
        let mut reason = LOST.to_string();
        loop {
            match read_frame::<_, ServerMsg>(&mut rd, &mut buf).await {
                Ok(Some(ServerMsg::Reply { id, result })) => {
                    let tx = reader_link
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                        .and_then(|m| m.remove(&id));
                    if let Some(tx) = tx {
                        let _ = tx.send(result);
                    }
                }
                Ok(Some(ServerMsg::State(s))) => {
                    let _ = events_tx.send(Event::State(s));
                }
                Ok(Some(ServerMsg::Notice { text, is_error })) => {
                    let _ = events_tx.send(Event::Notice { text, is_error });
                }
                Ok(Some(ServerMsg::Closing)) => reason = "the daemon stopped".into(),
                Ok(None) => break,
                Err(e) => {
                    reason = format!("{LOST} ({e})");
                    break;
                }
            }
        }
        // Fail every call still waiting, and refuse new ones.
        reader_link
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = events_tx.send(Event::Closed(reason));
    });

    Ok(Some(Connection {
        client: Client { link },
        welcome,
        events,
    }))
}

fn log_tail() -> String {
    let Ok(path) = proto::daemon_log_path() else {
        return String::new();
    };
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(12)..].join("\n")
}

/// Start `torrentui daemon run` detached from this terminal.
pub fn spawn_daemon(opts: &daemon::Options) -> Result<Child> {
    let log = proto::daemon_log_path()?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(&log).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = fs::remove_file(&log);
    }
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;

    let exe = std::env::current_exe().wrap_err("cannot find the torrentui executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run", "--config"])
        .arg(&opts.config_path);
    if let Some(dir) = &opts.dir {
        cmd.arg("--dir").arg(dir);
    }
    if opts.all_files {
        cmd.arg("--all-files");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        // Its own process group, so ^C in this terminal doesn't reach it.
        .process_group(0)
        .spawn()
        .wrap_err("cannot start the daemon")
}

/// Connect, starting the daemon first if it isn't running.
pub async fn connect_or_start(opts: &daemon::Options, subscribe: bool) -> Result<Connection> {
    if let Some(c) = connect(subscribe).await? {
        return Ok(c);
    }
    eprintln!("Starting the torrentui daemon…");
    let mut child = spawn_daemon(opts)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(c) = connect(subscribe).await? {
            return Ok(c);
        }
        if let Some(status) = child.try_wait()? {
            // Losing a start-up race to another launcher is fine: the
            // winner will be listening in a moment.
            let give_up = Instant::now() + Duration::from_secs(5);
            while Instant::now() < give_up {
                if let Some(c) = connect(subscribe).await? {
                    return Ok(c);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            bail!(
                "the daemon exited ({status}) while starting:\n{}",
                log_tail()
            );
        }
        if Instant::now() > deadline {
            bail!(
                "the daemon did not start within 60s; see {}",
                proto::daemon_log_path()?.display()
            );
        }
    }
}

/// Whether a daemon process currently holds the lock.
fn lock_held() -> Result<bool> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(proto::lock_path()?)?;
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Ask the daemon to stop and wait until it has flushed and exited.
/// Returns false if it wasn't running.
pub async fn stop() -> Result<bool> {
    let Some(conn) = connect(false).await? else {
        // Maybe it is still shutting down.
        return Ok(lock_held()? && wait_for_exit().await.map(|()| true)?);
    };
    conn.client.request(Request::Shutdown).await?;
    wait_for_exit().await?;
    Ok(true)
}

async fn wait_for_exit() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while lock_held()? {
        if Instant::now() > deadline {
            bail!("the daemon did not stop within 60s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}
