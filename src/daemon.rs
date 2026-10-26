//! The background daemon: the only process that owns the torrent session.
//!
//! It also owns the tags, the post-download actions and the speed limits, so
//! all of them keep working with no window open. TUI windows and `torrentui
//! open` are clients (see `client.rs`) talking to it over a private unix
//! socket (see `proto.rs`).

use std::{
    collections::BTreeMap,
    fs::{self, File, TryLockError},
    num::NonZeroU32,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    signal::unix::{SignalKind, signal},
    sync::{Notify, broadcast, mpsc},
};

use crate::{
    actions::{self, Context, Hook, HookSpec, Tracker},
    config::{self, Rate},
    desktop,
    engine::{Engine, EngineOptions, Prepared, Status, TorrentView},
    proto::{
        self, ClientMsg, DaemonStatus, PickerInfo, Reply, Request, ServerMsg, State, Welcome,
        write_frame,
    },
    tags::{self, Tags},
};

/// How often state is read from the engine and pushed to attached windows.
const TICK: Duration = Duration::from_millis(500);
/// Torrents allowed to wait for a file selection at once.
const MAX_PICKERS: usize = 64;
/// Adds allowed to resolve metadata at once.
const MAX_ADDING: usize = 32;

/// What `daemon run` is started with.
#[derive(Debug, Clone)]
pub struct Options {
    pub config_path: PathBuf,
    /// Overrides `download_dir` from the config.
    pub dir: Option<PathBuf>,
    /// Never show the file picker (overrides `ask_files`).
    pub all_files: bool,
}

struct Picker {
    name: String,
    /// Tags to give the torrent once the files are chosen.
    tags: Vec<String>,
    files: crate::engine::FileList,
    torrent_bytes: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    tags: Tags,
    /// The `on_complete` actions: the validated form the runner uses, and
    /// the spec form windows edit. Changed live by `SetHooks`.
    hooks: Vec<Hook>,
    hook_specs: Vec<HookSpec>,
    tracker: Tracker,
    pickers: BTreeMap<u64, Picker>,
    next_token: u64,
    adding: usize,
    actions_running: usize,
    limits: (Option<u32>, Option<u32>),
    /// Startup message, handed to the first window that asks.
    notice: Option<String>,
    /// Attached windows (clients that subscribed to state).
    uis: usize,
}

struct Inner {
    engine: Engine,
    log: PathBuf,
    tags_path: PathBuf,
    config_path: PathBuf,
    download_dir: PathBuf,
    ask_files: bool,
    notify: bool,
    uid: u32,
    started: Instant,
    shared: Mutex<Shared>,
    push: broadcast::Sender<Arc<ServerMsg>>,
    shutdown: Notify,
}

impl Inner {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        // Nothing here panics while holding the lock; if it ever did, the
        // data is still consistent enough to keep serving.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tell every attached window, or the desktop if there is none.
    fn notice(&self, text: impl Into<String>, is_error: bool) {
        let text = text.into();
        if self.shared().uis == 0 {
            if self.notify {
                desktop::notify("torrentui", &text);
            }
        } else {
            let _ = self
                .push
                .send(Arc::new(ServerMsg::Notice { text, is_error }));
        }
    }

    fn state(&self, torrents: Vec<TorrentView>) -> State {
        let s = self.shared();
        State {
            torrents,
            tags: s.tags.clone(),
            download_limit: s.limits.0,
            upload_limit: s.limits.1,
            pickers: s
                .pickers
                .iter()
                .map(|(token, p)| PickerInfo {
                    token: *token,
                    name: p.name.clone(),
                    files: p.files.clone(),
                })
                .collect(),
            adding: s.adding,
            actions_running: s.actions_running,
            hooks: s.hook_specs.clone(),
        }
    }

    fn save_tags(&self) -> Result<()> {
        let tags = self.shared().tags.clone();
        tags::save(&self.tags_path, &tags)
    }

    fn save_tracker(&self) {
        if let Err(e) = self.shared().tracker.save_if_dirty() {
            eprintln!("cannot save action state: {e:#}");
        }
    }
}

fn limit(n: Option<NonZeroU32>) -> Option<u32> {
    n.map(NonZeroU32::get)
}

/// Take the single-daemon lock. The OS drops it if we die, so there is no
/// stale-pid problem.
fn acquire_lock() -> Result<(File, u32)> {
    let path = proto::lock_path()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .wrap_err_with(|| format!("cannot open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("the torrentui daemon is already running"),
        Err(TryLockError::Error(e)) => return Err(e).wrap_err("cannot lock the daemon lock file"),
    }
    let uid = file.metadata()?.uid();
    Ok((file, uid))
}

fn bind() -> Result<UnixListener> {
    let path = proto::socket_path()?;
    // We hold the lock, so any socket file left here is stale.
    let _ = fs::remove_file(&path);
    let endpoint = proto::endpoint()?;
    let listener = UnixListener::bind(endpoint.path())
        .wrap_err_with(|| format!("cannot listen on {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Check what can be checked without starting: the config and its actions.
/// Used so mistakes are reported to the user's terminal, not a background log.
pub fn validate(opts: &Options) -> Result<()> {
    let cfg = config::load(&opts.config_path)?;
    actions::resolve(&cfg.on_complete)
        .wrap_err_with(|| format!("invalid on_complete in {}", opts.config_path.display()))?;
    if opts.dir.is_none() {
        cfg.download_dir()?;
    }
    Ok(())
}

/// Run the daemon until it is asked to stop.
pub async fn run(opts: Options) -> Result<()> {
    let cfg = config::load(&opts.config_path)?;
    let hooks = actions::resolve(&cfg.on_complete)
        .wrap_err_with(|| format!("invalid on_complete in {}", opts.config_path.display()))?;
    let download_dir = match &opts.dir {
        Some(d) => config::expand_tilde(&d.to_string_lossy()),
        None => cfg.download_dir()?,
    };
    let sessions_dir = config::sessions_dir()?;
    let tags_path = tags::default_path()?;
    let tags = tags::load(&tags_path)?;
    let (tracker, tracker_warning) = if hooks.is_empty() {
        (Tracker::default(), None)
    } else {
        Tracker::load()
    };

    let (_lock, uid) = acquire_lock()?;

    let migrated = match crate::engine::legacy_sessions_dir() {
        Some(legacy) => config::migrate_sessions(&legacy, &sessions_dir)?,
        None => false,
    };
    let engine = Engine::start(EngineOptions {
        download_dir: download_dir.clone(),
        sessions_dir: sessions_dir.clone(),
        download_bps: cfg.max_download_speed.bps(),
        upload_bps: cfg.max_upload_speed.bps(),
        peer_limit: (cfg.peer_limit > 0).then_some(cfg.peer_limit),
        dht: cfg.dht,
    })
    .await?;

    let notice = tracker_warning
        .or_else(|| migrated.then(|| format!("Moved session data to {}", sessions_dir.display())));
    if let Some(n) = &notice {
        eprintln!("{n}");
    }
    let (push, _) = broadcast::channel(16);
    let inner = Arc::new(Inner {
        engine,
        log: actions::log_path()?,
        tags_path,
        config_path: opts.config_path,
        download_dir,
        ask_files: cfg.ask_files && !opts.all_files,
        notify: cfg.notify,
        uid,
        started: Instant::now(),
        shared: Mutex::new(Shared {
            tags,
            hooks,
            hook_specs: cfg.on_complete.clone(),
            tracker,
            limits: (
                limit(cfg.max_download_speed.bps()),
                limit(cfg.max_upload_speed.bps()),
            ),
            notice,
            ..Shared::default()
        }),
        push,
        shutdown: Notify::new(),
    });

    let listener = bind()?;
    eprintln!("torrentui daemon {} listening", std::process::id());

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    // A closing terminal must not take the daemon down with it.
    let mut hup = signal(SignalKind::hangup())?;
    tokio::spawn(async move { while hup.recv().await.is_some() {} });

    let accept = tokio::spawn(accept_loop(inner.clone(), listener));
    let ticker = tokio::spawn(tick_loop(inner.clone()));
    tokio::select! {
        _ = inner.shutdown.notified() => eprintln!("shutdown requested"),
        _ = term.recv() => eprintln!("terminated"),
        _ = int.recv() => eprintln!("interrupted"),
    }

    accept.abort();
    ticker.abort();
    let _ = inner.push.send(Arc::new(ServerMsg::Closing));
    // Let the connection tasks deliver the goodbye before the session stops.
    tokio::time::sleep(Duration::from_millis(250)).await;
    inner.engine.shutdown().await;
    let _ = proto::socket_path().map(fs::remove_file);
    eprintln!("stopped");
    Ok(())
}

async fn accept_loop(inner: Arc<Inner>, listener: UnixListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // Transient accept errors (e.g. out of fds) shouldn't spin.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        // The directory is 0700 already; this also rejects other users if
        // someone loosened it.
        match stream.peer_cred() {
            Ok(c) if c.uid() == inner.uid => {}
            _ => continue,
        }
        tokio::spawn(handle_conn(inner.clone(), stream));
    }
}

/// Read the engine, fire post-download actions, and push state to windows.
async fn tick_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let views = inner.engine.snapshot();
        if !inner.shared().hooks.is_empty() {
            let finished = inner.shared().tracker.observe(&views);
            inner.save_tracker();
            for hash in finished {
                if let Some(view) = views.iter().find(|v| v.info_hash == hash) {
                    // Nothing to report if no action matches: that's normal.
                    let _ = spawn_actions(&inner, view.clone(), "auto");
                }
            }
        }
        if inner.shared().uis > 0 {
            let state = inner.state(views);
            let _ = inner.push.send(Arc::new(ServerMsg::State(Box::new(state))));
        }
    }
}

/// Run the actions matching a torrent's tags in the background.
fn spawn_actions(
    inner: &Arc<Inner>,
    view: TorrentView,
    trigger: &'static str,
) -> Result<(), String> {
    let tags = inner
        .shared()
        .tags
        .get(&view.info_hash)
        .cloned()
        .unwrap_or_default();
    let hooks: Vec<Hook> = inner
        .shared()
        .hooks
        .iter()
        .filter(|h| h.applies_to(&tags))
        .cloned()
        .collect();
    if hooks.is_empty() {
        return Err("No action matches this torrent's tags".into());
    }
    inner.shared().actions_running += 1;
    let inner = inner.clone();
    tokio::spawn(async move {
        let name = view.name.clone();
        let outcome = match inner.engine.content_path(view.id) {
            Ok(path) => {
                let ctx = Context::new(&view, path, &tags, trigger);
                Ok(actions::run_all(hooks, ctx, inner.log.clone()).await)
            }
            Err(e) => Err(format!("cannot locate the downloaded data: {e:#}")),
        };
        inner.shared().actions_running -= 1;
        match outcome {
            Err(e) => inner.notice(format!("Actions for {name} failed: {e}"), true),
            Ok(outcomes) => {
                let failed: Vec<String> = outcomes
                    .iter()
                    .filter_map(|o| o.result.as_ref().err().map(|e| format!("{}: {e}", o.label)))
                    .collect();
                if failed.is_empty() {
                    inner.notice(format!("Actions done: {name}"), false);
                } else {
                    inner.notice(
                        format!("Action failed — {} (see actions.log)", failed.join("; ")),
                        true,
                    );
                }
            }
        }
    });
    Ok(())
}

struct UiGuard(Arc<Inner>);

impl Drop for UiGuard {
    fn drop(&mut self) {
        self.0.shared().uis -= 1;
    }
}

async fn next_push(
    rx: &mut Option<broadcast::Receiver<Arc<ServerMsg>>>,
) -> Option<Result<Arc<ServerMsg>, broadcast::error::RecvError>> {
    match rx {
        Some(rx) => Some(rx.recv().await),
        None => std::future::pending().await,
    }
}

async fn handle_conn(inner: Arc<Inner>, stream: UnixStream) {
    let (rd, mut wr) = stream.into_split();

    // Requests are read by their own task: reading is not cancel-safe, so it
    // can't sit inside the select! below.
    let (tx, mut rx) = mpsc::channel::<ClientMsg>(32);
    tokio::spawn(async move {
        let mut rd = BufReader::new(rd);
        let mut buf = Vec::new();
        while let Ok(Some(msg)) = proto::read_frame::<_, ClientMsg>(&mut rd, &mut buf).await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let hello = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
    let Ok(Some(ClientMsg {
        id,
        req: Request::Hello { version, subscribe },
    })) = hello
    else {
        return;
    };
    if version != proto::VERSION {
        let result = Err(format!(
            "protocol mismatch: client speaks v{version}, the daemon speaks v{}; \
             restart the daemon with `torrentui daemon restart`",
            proto::VERSION
        ));
        let _ = write_frame(&mut wr, &ServerMsg::Reply { id, result }).await;
        return;
    }

    let _guard = subscribe.then(|| {
        inner.shared().uis += 1;
        UiGuard(inner.clone())
    });
    let mut push = subscribe.then(|| inner.push.subscribe());

    // Not inline in the literal below: that guard would live to the end of the
    // statement and deadlock the `notice` lock taken next to it.
    let has_actions = !inner.shared().hooks.is_empty();
    let welcome = Welcome {
        version: proto::VERSION,
        pid: std::process::id(),
        download_dir: inner.download_dir.display().to_string(),
        config_path: inner.config_path.display().to_string(),
        ask_files: inner.ask_files,
        has_actions,
        notice: if subscribe {
            inner.shared().notice.take()
        } else {
            None
        },
    };
    let reply = ServerMsg::Reply {
        id,
        result: Ok(Reply::Welcome(welcome)),
    };
    if write_frame(&mut wr, &reply).await.is_err() {
        return;
    }
    if subscribe {
        let state = ServerMsg::State(Box::new(inner.state(inner.engine.snapshot())));
        if write_frame(&mut wr, &state).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            req = rx.recv() => {
                let Some(ClientMsg { id, req }) = req else { break };
                let shutting_down = matches!(req, Request::Shutdown);
                let result = handle(&inner, req).await;
                if write_frame(&mut wr, &ServerMsg::Reply { id, result }).await.is_err() {
                    break;
                }
                if shutting_down {
                    inner.shutdown.notify_one();
                }
            }
            msg = next_push(&mut push) => match msg {
                Some(Ok(msg)) => {
                    if write_frame(&mut wr, &*msg).await.is_err() {
                        break;
                    }
                    if matches!(*msg, ServerMsg::Closing) {
                        break;
                    }
                }
                // A slow window missed some updates; the next full state
                // makes it current again.
                Some(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Some(Err(broadcast::error::RecvError::Closed)) | None => break,
            }
        }
    }
}

fn message(text: impl Into<String>, is_error: bool) -> Result<Reply, String> {
    Ok(Reply::Message {
        text: text.into(),
        is_error,
    })
}

async fn handle(inner: &Arc<Inner>, req: Request) -> Result<Reply, String> {
    let err = |e: color_eyre::Report| format!("{e:#}");
    match req {
        Request::Hello { .. } => Err("already greeted".into()),
        Request::Add { input, tags } => {
            let input = input.trim().to_string();
            if input.is_empty() {
                return Err("nothing to add".into());
            }
            // Never trust the client's normalisation.
            let tags = tags::parse_tags(&tags.join(" "))?;
            {
                let mut s = inner.shared();
                if s.adding >= MAX_ADDING {
                    return Err("too many torrents are still being added".into());
                }
                s.adding += 1;
            }
            tokio::spawn(do_add(inner.clone(), input, tags));
            Ok(Reply::Ok)
        }
        Request::TogglePause { id } => {
            inner.engine.toggle_pause(id).await.map_err(err)?;
            Ok(Reply::Ok)
        }
        Request::Remove { id, delete_files } => {
            let hash = inner
                .engine
                .snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .map(|t| t.info_hash);
            inner.engine.remove(id, delete_files).await.map_err(err)?;
            if let Some(hash) = hash {
                let had_tags = inner.shared().tags.remove(&hash).is_some();
                inner.shared().tracker.forget(&hash);
                inner.save_tracker();
                if had_tags && let Err(e) = inner.save_tags() {
                    return message(format!("Removed, but tags not saved: {e:#}"), true);
                }
            }
            message("Removed", false)
        }
        Request::Files { id } => {
            let (list, selected) = inner.engine.files(id).map_err(err)?;
            let mut selected: Vec<usize> = selected.into_iter().collect();
            selected.sort_unstable();
            Ok(Reply::Files { list, selected })
        }
        Request::SetFiles { id, only } => {
            if only.is_empty() {
                return Err("select at least one file".into());
            }
            inner.engine.set_files(id, only).await.map_err(err)?;
            message("File selection updated", false)
        }
        Request::ApplyPicker { token, only } => {
            if only.is_empty() {
                return Err("select at least one file".into());
            }
            let picker = inner.shared().pickers.remove(&token);
            let Some(picker) = picker else {
                return Err("that file selection was already handled in another window".into());
            };
            let name = picker.name.clone();
            match inner.engine.add_prepared(picker.torrent_bytes, only).await {
                Ok(added) => {
                    apply_new_tags(inner, &added.info_hash, &picker.tags);
                    message(format!("Added: {name}"), false)
                }
                Err(e) => Err(format!("Add failed: {e:#}")),
            }
        }
        Request::CancelPicker { token } => {
            let removed = inner.shared().pickers.remove(&token);
            match removed {
                Some(p) => message(format!("Cancelled: {}", p.name), false),
                None => Ok(Reply::Ok),
            }
        }
        Request::SetLimits { download, upload } => {
            let (d, u) = (
                download.and_then(NonZeroU32::new),
                upload.and_then(NonZeroU32::new),
            );
            inner.engine.set_limits(d, u);
            inner.shared().limits = (limit(d), limit(u));
            let (dr, ur) = (Rate(d), Rate(u));
            let saved = config::set_value(&inner.config_path, "max_download_speed", &dr.to_yaml())
                .and_then(|()| {
                    config::set_value(&inner.config_path, "max_upload_speed", &ur.to_yaml())
                });
            match saved {
                Ok(()) => message(
                    format!("Limits: down {}, up {} (saved)", dr.to_text(), ur.to_text()),
                    false,
                ),
                Err(e) => message(format!("Limits applied but not saved: {e:#}"), true),
            }
        }
        Request::SetTags { info_hash, tags } => {
            // Never trust the client's normalisation.
            let list = tags::parse_tags(&tags.join(" "))?;
            if !inner
                .engine
                .snapshot()
                .iter()
                .any(|t| t.info_hash == info_hash)
            {
                return Err("that torrent no longer exists".into());
            }
            {
                let mut s = inner.shared();
                if list.is_empty() {
                    s.tags.remove(&info_hash);
                } else {
                    s.tags.insert(info_hash, list.clone());
                }
            }
            match inner.save_tags() {
                Ok(()) if list.is_empty() => message("Tags cleared", false),
                Ok(()) => message(format!("Tags: {}", list.join(", ")), false),
                Err(e) => message(format!("Tags applied but not saved: {e:#}"), true),
            }
        }
        Request::DeleteTag { tag } => {
            let tag = match tags::parse_tags(&tag)?.as_slice() {
                [one] => one.clone(),
                _ => return Err("give exactly one tag".into()),
            };
            let (mut new_specs, mut new_tags) = {
                let s = inner.shared();
                (s.hook_specs.clone(), s.tags.clone())
            };
            // An action limited to just this tag would start running for
            // everything, so make the user decide about it first.
            if let Some(h) = new_specs.iter().find(|h| h.tags == [tag.clone()]) {
                let label = h
                    .name
                    .clone()
                    .unwrap_or_else(|| actions::join_command(&h.run));
                return Err(format!(
                    "Action \"{label}\" only runs for #{tag}: edit or delete it first"
                ));
            }
            let mut actions_changed = 0;
            for h in new_specs.iter_mut().filter(|h| h.tags.contains(&tag)) {
                h.tags.retain(|t| *t != tag);
                actions_changed += 1;
            }
            let mut torrents = 0;
            for list in new_tags.values_mut() {
                if list.contains(&tag) {
                    list.retain(|t| *t != tag);
                    torrents += 1;
                }
            }
            new_tags.retain(|_, v| !v.is_empty());
            if actions_changed > 0 {
                let resolved = actions::resolve(&new_specs).map_err(|e| format!("{e:#}"))?;
                config::set_on_complete(&inner.config_path, &new_specs)
                    .map_err(|e| format!("cannot save {}: {e:#}", inner.config_path.display()))?;
                let mut s = inner.shared();
                s.hooks = resolved;
                s.hook_specs = new_specs;
            }
            inner.shared().tags = new_tags;
            let summary =
                format!("Deleted #{tag} ({torrents} torrent(s), {actions_changed} action(s))");
            match inner.save_tags() {
                Ok(()) => message(summary, false),
                Err(e) => message(format!("{summary}, but tags not saved: {e:#}"), true),
            }
        }
        Request::SetHooks { hooks } => {
            let resolved = actions::resolve(&hooks).map_err(|e| format!("{e:#}"))?;
            config::set_on_complete(&inner.config_path, &hooks)
                .map_err(|e| format!("cannot save {}: {e:#}", inner.config_path.display()))?;
            let n = hooks.len();
            {
                let mut s = inner.shared();
                s.hooks = resolved;
                s.hook_specs = hooks;
            }
            message(
                match n {
                    0 => "Actions cleared".to_string(),
                    1 => "1 action saved".to_string(),
                    n => format!("{n} actions saved"),
                },
                false,
            )
        }
        Request::RunActions { id } => {
            let view = inner
                .engine
                .snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .ok_or("that torrent no longer exists")?;
            let finished = view.status == Status::Seeding
                || (view.total_bytes > 0 && view.progress_bytes >= view.total_bytes);
            if !finished {
                return Err("Actions run on finished torrents".into());
            }
            spawn_actions(inner, view, "manual")?;
            Ok(Reply::Ok)
        }
        Request::Status => {
            let uis = inner.shared().uis;
            Ok(Reply::DaemonStatus(DaemonStatus {
                pid: std::process::id(),
                torrents: inner.engine.snapshot().len(),
                uis,
                uptime_secs: inner.started.elapsed().as_secs(),
            }))
        }
        Request::Shutdown => Ok(Reply::Ok),
    }
}

/// Give a just-added torrent its tags, keeping any it already has.
fn apply_new_tags(inner: &Inner, info_hash: &str, new: &[String]) {
    if new.is_empty() {
        return;
    }
    {
        let mut s = inner.shared();
        let list = s.tags.entry(info_hash.to_string()).or_default();
        list.extend(new.iter().cloned());
        list.sort();
        list.dedup();
    }
    if let Err(e) = inner.save_tags() {
        inner.notice(format!("Tags applied but not saved: {e:#}"), true);
    }
}

/// Resolve a torrent's metadata and either start it or park it for a file
/// selection. Runs detached: magnets can take a while.
async fn do_add(inner: Arc<Inner>, input: String, tags: Vec<String>) {
    enum Done {
        Added(crate::engine::Added),
        Pick(Prepared),
    }
    let outcome: Result<Done> = async {
        if inner.ask_files {
            let p = inner.engine.prepare(&input).await?;
            if p.files.entries.len() > 1 {
                return Ok(Done::Pick(p));
            }
            let added = inner
                .engine
                .add_prepared(p.torrent_bytes, p.files.all_indices())
                .await?;
            Ok(Done::Added(added))
        } else {
            Ok(Done::Added(inner.engine.add(&input).await?))
        }
    }
    .await;
    inner.shared().adding -= 1;

    match outcome {
        Ok(Done::Added(added)) => {
            apply_new_tags(&inner, &added.info_hash, &tags);
            inner.notice(format!("Added: {}", added.name), false);
        }
        Ok(Done::Pick(p)) => {
            let waiting = {
                let mut s = inner.shared();
                if s.pickers.len() >= MAX_PICKERS {
                    None
                } else {
                    s.next_token += 1;
                    let token = s.next_token;
                    s.pickers.insert(
                        token,
                        Picker {
                            name: p.name.clone(),
                            tags: tags.clone(),
                            files: p.files,
                            torrent_bytes: p.torrent_bytes,
                        },
                    );
                    Some(s.uis == 0)
                }
            };
            match waiting {
                // With no window open, say that something is waiting.
                Some(true) => inner.notice(
                    format!("Choose files for {}: open torrentui", p.name),
                    false,
                ),
                Some(false) => {}
                None => inner.notice("Too many torrents are waiting for a file selection", true),
            }
        }
        Err(e) => inner.notice(format!("Add failed: {e:#}"), true),
    }
}
