//! Post-download actions: user-configured commands run when a torrent finishes.
//!
//! Safety model: the commands come only from the user's own config file and
//! are executed directly, never through a shell. Torrent-controlled text (the
//! name) is only ever substituted into arguments or environment variables, so
//! it cannot change which program runs or split into extra arguments.
//!
//! This module has the pure pieces (parsing, matching, placeholder expansion,
//! completion tracking) and the async runner. The event loop wires them up.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use serde::Deserialize;

use crate::{
    config,
    engine::{Status, TorrentView},
    tags,
};

const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// The log is moved to `actions.log.1` once it grows past this.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// One `on_complete` entry as written in the config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    /// Label shown in messages and the log; defaults to the program name.
    pub name: Option<String>,
    /// The program followed by its arguments. Placeholders `{path}`, `{name}`,
    /// `{hash}` and `{tags}` are replaced in the arguments.
    pub run: Vec<String>,
    /// Only run for torrents having at least one of these tags (empty: all).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Kill the command after this many seconds; 0 means no limit.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

/// A validated action.
#[derive(Debug, Clone)]
pub struct Hook {
    pub label: String,
    program: OsString,
    args: Vec<String>,
    tags: Vec<String>,
    timeout: Option<Duration>,
}

/// Validate the configured actions. Errors name the offending entry.
pub fn resolve(specs: &[HookSpec]) -> Result<Vec<Hook>> {
    specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            resolve_one(spec).wrap_err_with(|| format!("invalid on_complete entry #{}", i + 1))
        })
        .collect()
}

fn resolve_one(spec: &HookSpec) -> Result<Hook> {
    let Some(program) = spec.run.first().map(|p| p.trim()).filter(|p| !p.is_empty()) else {
        bail!("`run` must start with the program to execute");
    };
    // A relative path would be resolved against the torrent's folder, which is
    // never what anyone means. Bare names are looked up on PATH instead.
    let program = config::expand_tilde(program);
    if program.components().count() > 1 && !program.is_absolute() {
        bail!(
            "program '{}' must be an absolute path, start with ~/, or be a bare \
             command name found on PATH",
            program.display()
        );
    }
    let tags = tags::parse_tags(&spec.tags.join(" ")).map_err(|e| eyre!("{e}"))?;
    let label = match spec.name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.display().to_string()),
    };
    Ok(Hook {
        label,
        program: program.into_os_string(),
        args: spec.run[1..].to_vec(),
        tags,
        timeout: (spec.timeout_secs > 0).then(|| Duration::from_secs(spec.timeout_secs)),
    })
}

impl Hook {
    /// Whether this action applies to a torrent with these tags.
    pub fn applies_to(&self, torrent_tags: &[String]) -> bool {
        self.tags.is_empty() || self.tags.iter().any(|t| torrent_tags.contains(t))
    }
}

/// Everything an action is told about a finished torrent.
#[derive(Debug, Clone)]
pub struct Context {
    pub name: String,
    pub info_hash: String,
    /// The downloaded data: the file for a single-file torrent, else the folder.
    pub path: PathBuf,
    /// The torrent's output folder; also the command's working directory.
    pub dir: PathBuf,
    pub tags: Vec<String>,
    /// "auto" (finished downloading) or "manual" (run with `x`).
    pub trigger: &'static str,
}

impl Context {
    pub fn new(view: &TorrentView, path: PathBuf, tags: &[String], trigger: &'static str) -> Self {
        Self {
            // Environment variables and arguments can't hold NUL bytes, and a
            // torrent name is attacker-controlled.
            name: view.name.replace('\0', "\u{fffd}"),
            info_hash: view.info_hash.clone(),
            path,
            dir: view.output_folder.clone(),
            tags: tags.to_vec(),
            trigger,
        }
    }

    fn tags_csv(&self) -> String {
        self.tags.join(",")
    }
}

/// Replace `{path}`, `{name}`, `{hash}` and `{tags}` in one pass, so text
/// that came from a substituted value is never expanded again. Other braces
/// are left alone.
fn expand(arg: &str, ctx: &Context) -> OsString {
    let mut out = OsString::new();
    let mut rest = arg;
    while let Some(i) = rest.find('{') {
        out.push(&rest[..i]);
        let after = &rest[i..];
        let (value, len): (OsString, usize) = if after.starts_with("{path}") {
            (ctx.path.clone().into_os_string(), 6)
        } else if after.starts_with("{name}") {
            (ctx.name.clone().into(), 6)
        } else if after.starts_with("{hash}") {
            (ctx.info_hash.clone().into(), 6)
        } else if after.starts_with("{tags}") {
            (ctx.tags_csv().into(), 6)
        } else {
            out.push("{");
            rest = &after[1..];
            continue;
        };
        out.push(value);
        rest = &after[len..];
    }
    out.push(rest);
    out
}

/// Result of one action.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub label: String,
    /// `Err` holds a short human-readable reason.
    pub result: Result<(), String>,
}

/// Run the given actions one after another (so they can build on each other),
/// carrying on after a failure. Output goes to `log`.
pub async fn run_all(hooks: Vec<Hook>, ctx: Context, log: PathBuf) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(hooks.len());
    for hook in hooks {
        let result = run_one(&hook, &ctx, &log).await;
        outcomes.push(Outcome {
            label: hook.label,
            result,
        });
    }
    outcomes
}

async fn run_one(hook: &Hook, ctx: &Context, log: &Path) -> Result<(), String> {
    if !ctx.dir.is_dir() {
        return Err(format!("folder {} does not exist", ctx.dir.display()));
    }
    let args: Vec<OsString> = hook.args.iter().map(|a| expand(a, ctx)).collect();

    let mut log_file = open_log(log).ok();
    if let Some(f) = log_file.as_mut() {
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let _ = writeln!(
            f,
            "[{}] {} — {} ({}): {} {:?}",
            timestamp(SystemTime::now()),
            hook.label,
            ctx.name,
            ctx.trigger,
            hook.program.to_string_lossy(),
            shown
        );
    }
    let stdio = |f: &Option<fs::File>| match f.as_ref().and_then(|f| f.try_clone().ok()) {
        Some(f) => Stdio::from(f),
        None => Stdio::null(),
    };

    let mut cmd = tokio::process::Command::new(&hook.program);
    cmd.args(&args)
        .current_dir(&ctx.dir)
        .env("TORRENTUI_PATH", &ctx.path)
        .env("TORRENTUI_DIR", &ctx.dir)
        .env("TORRENTUI_NAME", &ctx.name)
        .env("TORRENTUI_HASH", &ctx.info_hash)
        .env("TORRENTUI_TAGS", ctx.tags_csv())
        .env("TORRENTUI_TRIGGER", ctx.trigger)
        .stdin(Stdio::null())
        .stdout(stdio(&log_file))
        .stderr(stdio(&log_file));
    // Deliberately no kill_on_drop: quitting torrentui must not interrupt a
    // script that is halfway through moving files.

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", Path::new(&hook.program).display()))?;
    let waited = match hook.timeout {
        Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
            Ok(r) => r,
            Err(_) => {
                let _ = child.kill().await;
                note(
                    &mut log_file,
                    &format!("-> killed after {}s", limit.as_secs()),
                );
                return Err(format!("timed out after {}s", limit.as_secs()));
            }
        },
        None => child.wait().await,
    };
    match waited {
        Ok(status) if status.success() => {
            note(&mut log_file, "-> ok");
            Ok(())
        }
        Ok(status) => {
            note(&mut log_file, &format!("-> {status}"));
            Err(format!("{status}"))
        }
        Err(e) => Err(format!("waiting for it failed: {e}")),
    }
}

fn note(file: &mut Option<fs::File>, text: &str) {
    if let Some(f) = file.as_mut() {
        let _ = writeln!(f, "{text}");
    }
}

fn open_log(path: &Path) -> std::io::Result<fs::File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::rename(path, old);
    }
    fs::OpenOptions::new().create(true).append(true).open(path)
}

/// `$XDG_DATA_HOME/torrentui/actions.log`.
pub fn log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("actions.log"))
}

/// Which torrents were seen downloading and have not been acted on yet.
fn state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("pending-actions.yml"))
}

fn data_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| eyre!("cannot determine your data directory"))?
        .join("torrentui"))
}

/// UTC `YYYY-MM-DD HH:MM:SS`.
fn timestamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Decides when a torrent has just finished downloading.
///
/// A torrent that is already complete when torrentui starts must not fire, but
/// it also shows up as "checking" and then "seeding" without ever
/// "downloading". So a torrent is remembered (on disk, so it survives a
/// restart) once it has been seen downloading, and fires the first time it is
/// seen finished after that.
#[derive(Debug, Default)]
pub struct Tracker {
    pending: BTreeSet<String>,
    dirty: bool,
}

impl Tracker {
    /// Load the saved state. A missing file is empty. An unreadable one is
    /// also treated as empty, with a message for the status bar: this is
    /// derived state, and refusing to start over it would be worse.
    pub fn load() -> (Self, Option<String>) {
        let path = match state_path() {
            Ok(p) => p,
            Err(e) => return (Self::default(), Some(format!("{e:#}"))),
        };
        match Self::load_from(&path) {
            Ok(t) => (t, None),
            Err(e) => (
                Self::default(),
                Some(format!("Ignoring unreadable {}: {e:#}", path.display())),
            ),
        }
    }

    fn load_from(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let list: Option<Vec<String>> = serde_yaml_ng::from_str(&text)?;
        Ok(Self {
            pending: list.unwrap_or_default().into_iter().collect(),
            dirty: false,
        })
    }

    /// Feed a fresh snapshot; returns the info-hashes that just finished.
    pub fn observe(&mut self, torrents: &[TorrentView]) -> Vec<String> {
        let mut finished = Vec::new();
        for t in torrents {
            match t.status {
                Status::Downloading if t.total_bytes > 0 => {
                    self.dirty |= self.pending.insert(t.info_hash.clone());
                }
                Status::Seeding if self.pending.remove(&t.info_hash) => {
                    self.dirty = true;
                    finished.push(t.info_hash.clone());
                }
                _ => {}
            }
        }
        finished
    }

    pub fn forget(&mut self, info_hash: &str) {
        self.dirty |= self.pending.remove(info_hash);
    }

    /// Write the state if it changed since the last save.
    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        let list: Vec<&String> = self.pending.iter().collect();
        let body = if list.is_empty() {
            String::new()
        } else {
            serde_yaml_ng::to_string(&list)?
        };
        config::write_atomic(
            &state_path()?,
            &format!("# torrentui: downloads awaiting their on_complete actions.\n{body}"),
        )
    }

    #[cfg(test)]
    fn pending(&self) -> Vec<&str> {
        self.pending.iter().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(run: &[&str], tags: &[&str]) -> HookSpec {
        HookSpec {
            name: None,
            run: run.iter().map(|s| s.to_string()).collect(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 5,
        }
    }

    fn view(hash: &str, status: Status) -> TorrentView {
        TorrentView {
            id: 1,
            name: "Some Movie".into(),
            info_hash: hash.into(),
            output_folder: std::env::temp_dir(),
            status,
            progress_bytes: 0,
            total_bytes: 100,
            uploaded_bytes: 0,
            down_bps: 0,
            up_bps: 0,
            peers_live: 0,
            peers_connecting: 0,
            peers_queued: 0,
            peers_seen: 0,
            eta: None,
            error: None,
        }
    }

    fn ctx(dir: &Path) -> Context {
        Context {
            name: "My {hash} Movie".into(),
            info_hash: "abc123".into(),
            path: dir.join("My Movie"),
            dir: dir.to_path_buf(),
            tags: vec!["4k".into(), "movies".into()],
            trigger: "auto",
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("torrentui-actions-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_accepts_good_entries() {
        let hooks = resolve(&[
            spec(&["~/bin/sort.sh", "{path}"], &["Movies"]),
            spec(&["notify-send", "done"], &[]),
        ])
        .unwrap();
        assert_eq!(hooks[0].label, "sort.sh");
        assert_eq!(hooks[0].tags, ["movies"]);
        assert!(hooks[0].program.to_string_lossy().ends_with("/bin/sort.sh"));
        assert!(!hooks[0].program.to_string_lossy().starts_with('~'));
        assert_eq!(hooks[1].label, "notify-send");
        assert_eq!(hooks[1].timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn resolve_rejects_bad_entries_naming_the_entry() {
        let e = |s: HookSpec| format!("{:#}", resolve(&[spec(&["ok"], &[]), s]).unwrap_err());
        assert!(e(spec(&[], &[])).contains("#2") && e(spec(&[], &[])).contains("program"));
        assert!(e(spec(&["  "], &[])).contains("program"));
        assert!(e(spec(&["./run.sh"], &[])).contains("absolute"));
        assert!(e(spec(&["bin/run.sh"], &[])).contains("absolute"));
        assert!(e(spec(&["ok"], &["bad tag!"])).contains("'!'"));
        let mut named = spec(&["ok"], &[]);
        named.name = Some("  my action ".into());
        assert_eq!(resolve(&[named]).unwrap()[0].label, "my action");
    }

    #[test]
    fn timeout_zero_means_unlimited() {
        let mut s = spec(&["ok"], &[]);
        s.timeout_secs = 0;
        assert_eq!(resolve(&[s]).unwrap()[0].timeout, None);
    }

    #[test]
    fn tag_matching() {
        let hooks = resolve(&[spec(&["a"], &[]), spec(&["b"], &["movies", "tv"])]).unwrap();
        let tags = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(hooks[0].applies_to(&[]));
        assert!(!hooks[1].applies_to(&[]));
        assert!(!hooks[1].applies_to(&tags(&["music"])));
        assert!(hooks[1].applies_to(&tags(&["music", "tv"])));
    }

    #[test]
    fn expands_known_placeholders_in_one_pass() {
        let c = ctx(Path::new("/dl"));
        let e = |s: &str| expand(s, &c).to_string_lossy().into_owned();
        assert_eq!(e("{path}"), "/dl/My Movie");
        assert_eq!(e("--tags={tags}"), "--tags=4k,movies");
        assert_eq!(e("{hash}:{path}"), "abc123:/dl/My Movie");
        // The name contains "{hash}" but substituted text is not re-expanded.
        assert_eq!(e("{name}"), "My {hash} Movie");
        // Unknown and unbalanced braces are literal.
        assert_eq!(e("{x} {{path}} { {"), "{x} {/dl/My Movie} { {");
        assert_eq!(e("plain"), "plain");
        assert_eq!(e(""), "");
    }

    #[test]
    fn context_strips_nul_from_names() {
        let mut v = view("h", Status::Seeding);
        v.name = "a\0b".into();
        let c = Context::new(&v, "/x".into(), &[], "auto");
        assert_eq!(c.name, "a\u{fffd}b");
    }

    #[test]
    fn timestamps_are_utc_dates() {
        let at = |s: u64| timestamp(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01 00:00:00");
        assert_eq!(at(951_782_400 + 86_399), "2000-02-29 23:59:59");
        assert_eq!(at(1_761_500_000), "2025-10-26 17:33:20");
    }

    #[test]
    fn tracker_fires_once_after_seeing_a_download() {
        let mut t = Tracker::default();
        assert!(t.observe(&[view("a", Status::Initializing)]).is_empty());
        assert!(t.observe(&[view("a", Status::Downloading)]).is_empty());
        assert_eq!(t.pending(), ["a"]);
        assert_eq!(t.observe(&[view("a", Status::Seeding)]), ["a"]);
        assert!(t.observe(&[view("a", Status::Seeding)]).is_empty());
        assert!(t.pending().is_empty());
    }

    #[test]
    fn tracker_ignores_torrents_that_were_already_complete() {
        let mut t = Tracker::default();
        t.observe(&[view("done", Status::Initializing)]);
        assert!(t.observe(&[view("done", Status::Seeding)]).is_empty());
        // Pausing and resuming a finished torrent doesn't fire either.
        t.observe(&[view("done", Status::Paused)]);
        assert!(t.observe(&[view("done", Status::Seeding)]).is_empty());
    }

    #[test]
    fn tracker_survives_pause_and_forgets_removed_torrents() {
        let mut t = Tracker::default();
        t.observe(&[
            view("a", Status::Downloading),
            view("b", Status::Downloading),
        ]);
        t.observe(&[view("a", Status::Paused)]);
        t.forget("b");
        assert_eq!(t.pending(), ["a"]);
        assert_eq!(t.observe(&[view("a", Status::Seeding)]), ["a"]);
    }

    #[test]
    fn tracker_state_round_trips_through_the_file() {
        let dir = scratch("tracker");
        let path = dir.join("state.yml");
        assert!(Tracker::load_from(&path).unwrap().pending.is_empty());

        let mut t = Tracker::default();
        t.observe(&[view(
            "1234567890123456789012345678901234567890",
            Status::Downloading,
        )]);
        // Saving goes through the real data dir, so check the format directly.
        let list: Vec<&String> = t.pending.iter().collect();
        fs::write(&path, serde_yaml_ng::to_string(&list).unwrap()).unwrap();
        let back = Tracker::load_from(&path).unwrap();
        assert_eq!(back.pending, t.pending);

        fs::write(&path, "{ not a list").unwrap();
        assert!(Tracker::load_from(&path).is_err());
        fs::write(&path, "# only a comment\n").unwrap();
        assert!(Tracker::load_from(&path).unwrap().pending.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    fn sh(script: &str, tags: &[&str], timeout: u64) -> Hook {
        let mut s = spec(
            &["/bin/sh", "-c", script, "sh", "{path}", "{tags}", "{name}"],
            tags,
        );
        s.timeout_secs = timeout;
        s.name = Some("test".into());
        resolve(&[s]).unwrap().remove(0)
    }

    #[tokio::test]
    async fn runs_with_args_env_cwd_and_logs_output() {
        let dir = scratch("run");
        let log = dir.join("logs").join("actions.log");
        let script = r#"
            printf '%s\n' "$1" "$2" "$3" "$TORRENTUI_PATH" "$TORRENTUI_TAGS" "$TORRENTUI_NAME" \
                "$TORRENTUI_HASH" "$TORRENTUI_DIR" "$TORRENTUI_TRIGGER" "$(pwd)" > out.txt
            echo to-stdout; echo to-stderr >&2
        "#;
        let c = ctx(&dir);
        let out = run_all(vec![sh(script, &[], 10)], c.clone(), log.clone()).await;
        assert!(out[0].result.is_ok(), "{:?}", out[0].result);
        assert_eq!(out[0].label, "test");

        let lines: Vec<String> = fs::read_to_string(dir.join("out.txt"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        let p = c.path.to_string_lossy().into_owned();
        let d = fs::canonicalize(&dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(lines[0], p);
        assert_eq!(lines[1], "4k,movies");
        // Special characters in the name arrive verbatim, unexpanded and unsplit.
        assert_eq!(lines[2], "My {hash} Movie");
        assert_eq!(lines[3], p);
        assert_eq!(lines[4], "4k,movies");
        assert_eq!(lines[5], "My {hash} Movie");
        assert_eq!(lines[6], "abc123");
        assert_eq!(lines[7], dir.to_string_lossy());
        assert_eq!(lines[8], "auto");
        assert_eq!(lines[9], d);

        let logged = fs::read_to_string(&log).unwrap();
        assert!(
            logged.contains("to-stdout") && logged.contains("to-stderr"),
            "{logged}"
        );
        assert!(logged.contains("test — My {hash} Movie (auto)") && logged.contains("-> ok"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn names_cannot_inject_commands() {
        let dir = scratch("inject");
        let mut c = ctx(&dir);
        c.name = "x; touch pwned; $(touch pwned2) `touch pwned3`".into();
        let hook = sh("true", &[], 10);
        let out = run_all(vec![hook], c, dir.join("l.log")).await;
        assert!(out[0].result.is_ok());
        assert!(!dir.join("pwned").exists() && !dir.join("pwned2").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reports_failures_and_keeps_going() {
        let dir = scratch("fail");
        let log = dir.join("a.log");
        let hooks = vec![
            sh("exit 3", &[], 10),
            sh("touch second-ran", &[], 10),
            resolve(&[spec(&["/definitely/not/here"], &[])])
                .unwrap()
                .remove(0),
        ];
        let out = run_all(hooks, ctx(&dir), log.clone()).await;
        assert!(
            out[0].result.as_ref().unwrap_err().contains("3"),
            "{:?}",
            out[0]
        );
        assert!(out[1].result.is_ok());
        assert!(dir.join("second-ran").exists());
        assert!(out[2].result.as_ref().unwrap_err().contains("cannot start"));
        assert!(fs::read_to_string(&log).unwrap().contains("exit status: 3"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn kills_commands_that_time_out() {
        let dir = scratch("timeout");
        let started = std::time::Instant::now();
        let out = run_all(vec![sh("sleep 30", &[], 1)], ctx(&dir), dir.join("a.log")).await;
        assert!(
            out[0]
                .result
                .as_ref()
                .unwrap_err()
                .contains("timed out after 1s")
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_folder_is_an_error_not_a_misleading_spawn_failure() {
        let dir = scratch("nodir");
        let mut c = ctx(&dir);
        c.dir = dir.join("gone");
        let out = run_all(vec![sh("true", &[], 5)], c, dir.join("a.log")).await;
        assert!(
            out[0]
                .result
                .as_ref()
                .unwrap_err()
                .contains("does not exist")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_rotates_when_large() {
        let dir = scratch("rotate");
        let log = dir.join("actions.log");
        fs::write(&log, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        open_log(&log).unwrap();
        assert!(dir.join("actions.log.1").exists());
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
