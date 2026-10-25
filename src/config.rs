//! Configuration file (`~/.config/torrentui/config.yml`).
//!
//! Loading is strict on purpose: unknown keys, bad values and unknown themes
//! are startup errors that name the file and the problem, rather than being
//! silently ignored.

use std::{
    collections::BTreeMap,
    fs,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use serde::Deserialize;

use crate::{actions::HookSpec, theme::ThemeSpec};

pub const DEFAULT_CONFIG: &str = "\
# torrentui configuration.
# Everything here is optional; delete a line to get its default.

# Where new torrents are saved. Accepts ~ . Default: your Downloads folder.
# download_dir: ~/Downloads

# Colour theme: default, tokyo-night, catppuccin-frappe, catppuccin-mocha,
# dracula, one-dark, or the name of a theme defined under `themes:` below.
# Everything except `default` uses 24-bit colour.  Press `t` in the app to
# preview and pick a theme; the choice is saved here.
theme: default

# Speed limits. Use a unit: B, KiB, MiB or GiB, optionally followed by /s
# (e.g. 500KiB/s, 2MiB/s). 0 means unlimited. Press `l` in the app to edit.
max_download_speed: 0
max_upload_speed: 0

# Maximum peers connected per torrent. 0 uses the engine default.
peer_limit: 0

# Use the DHT (trackerless peer discovery). Needed for most magnet links.
dht: true

# Offer a file picker for multi-file torrents before downloading.
ask_files: true

# Desktop notifications (via notify-send) from the background daemon when no
# torrentui window is open: torrents added from a browser, failures, and
# torrents waiting for a file selection.
notify: true

# Commands to run when a torrent finishes downloading. You can also run them
# by hand on a finished torrent with `x`. No shell is involved: `run` is the
# program followed by its arguments, and {path} {name} {hash} {tags} in the
# arguments are replaced (tags are comma-separated). The same values are in
# $TORRENTUI_PATH, $TORRENTUI_NAME, $TORRENTUI_HASH and $TORRENTUI_TAGS. With
# `tags`, only torrents that have at least one of those tags run it. Output is
# appended to ~/.local/share/torrentui/actions.log. Read at startup.
#
# on_complete:
#   - name: sort movies
#     run: [\"~/bin/sort-movies.sh\", \"{path}\", \"{tags}\"]
#     tags: [movies]
#     timeout_secs: 600     # 0 = no limit

# Your own themes. Every colour is optional and inherits from `base`
# (one of the built-in themes, default: `default`). Colours are #rrggbb,
# an ANSI name such as `red`, or a 0-255 index.
#
# themes:
#   my-theme:
#     base: dracula
#     accent: \"#ff79c6\"
#     bg: \"#101010\"
#   # Keys: bg fg muted accent border selection_bg success info warning error special
";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub download_dir: Option<String>,
    pub theme: String,
    pub max_download_speed: Rate,
    pub max_upload_speed: Rate,
    pub peer_limit: usize,
    pub dht: bool,
    pub ask_files: bool,
    pub notify: bool,
    pub on_complete: Vec<HookSpec>,
    pub themes: BTreeMap<String, ThemeSpec>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: None,
            theme: "default".into(),
            max_download_speed: Rate::default(),
            max_upload_speed: Rate::default(),
            peer_limit: 0,
            dht: true,
            ask_files: true,
            notify: true,
            on_complete: Vec::new(),
            themes: BTreeMap::new(),
        }
    }
}

impl Config {
    /// The directory downloads go to: `download_dir` with `~` expanded, or the
    /// platform Downloads folder.
    pub fn download_dir(&self) -> Result<PathBuf> {
        match &self.download_dir {
            Some(d) => {
                let p = expand_tilde(d.trim());
                if !p.is_absolute() {
                    bail!(
                        "download_dir '{d}' must be an absolute path or start with ~/ \
                         (relative paths depend on where you launch from)"
                    );
                }
                Ok(p)
            }
            // Never a relative path: the daemon's working directory is arbitrary.
            None => dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .ok_or_else(|| {
                    eyre!("cannot determine a downloads folder; set download_dir in the config")
                }),
        }
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// `$XDG_CONFIG_HOME/torrentui/config.yml`, which is
/// `~/.config/torrentui/config.yml` unless you have moved XDG_CONFIG_HOME.
pub fn default_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => dirs::home_dir()
            .ok_or_else(|| eyre!("cannot determine your home directory"))?
            .join(".config"),
    };
    Ok(base.join("torrentui").join("config.yml"))
}

/// `$XDG_DATA_HOME/torrentui/sessions` (`~/.local/share/torrentui/sessions`):
/// the torrent list, resume data and cached `.torrent` files.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| eyre!("cannot determine your data directory"))?
        .join("torrentui")
        .join("sessions"))
}

/// Earlier versions kept session data in the engine's own default folder.
/// Move it to `new` the first time, so existing torrents carry over. Does
/// nothing if `new` already exists (never merges) or `legacy` doesn't.
/// Returns whether anything was moved.
pub fn migrate_sessions(legacy: &Path, new: &Path) -> Result<bool> {
    if new.exists() || !legacy.is_dir() {
        return Ok(false);
    }
    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("cannot create {}", parent.display()))?;
    }
    fs::rename(legacy, new).wrap_err_with(|| {
        format!(
            "cannot move session data from {} to {}; move it by hand to keep your torrents",
            legacy.display(),
            new.display()
        )
    })?;
    Ok(true)
}

/// Parse config text. A file with only comments/whitespace is the defaults.
pub fn parse(text: &str) -> Result<Config, serde_yaml_ng::Error> {
    let blank = text
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
    if blank {
        return Ok(Config::default());
    }
    serde_yaml_ng::from_str(text)
}

/// Load the config, creating a commented default file if none exists yet.
pub fn load(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text).wrap_err_with(|| format!("invalid config {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Best effort: not being able to write the template isn't fatal.
            let _ = write_atomic(path, DEFAULT_CONFIG);
            Ok(Config::default())
        }
        Err(e) => Err(e).wrap_err_with(|| format!("cannot read config {}", path.display())),
    }
}

/// Write via a temp file + rename so a crash can't leave a half-written
/// config. Follows a symlinked config file (e.g. managed by stow) instead of
/// replacing the link.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| eyre!("config path has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&tmp, text)?;
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Set a top-level `key: value` in the file, keeping every other line (and
/// comments) untouched. `value` is raw YAML. The edit is checked to still
/// parse before anything is written.
pub fn set_value(path: &Path, key: &str, value: &str) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_string(),
        Err(e) => return Err(e).wrap_err_with(|| format!("cannot read {}", path.display())),
    };
    let updated = replace_key(&existing, key, value);
    parse(&updated).wrap_err_with(|| format!("refusing to write invalid {key}: {value}"))?;
    write_atomic(path, &updated)
}

fn replace_key(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}:");
    let mut replaced = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && line.starts_with(&prefix) {
                replaced = true;
                // Keep a trailing comment (and the spacing before it), if any.
                let rest = &line[prefix.len()..];
                match rest.find(" #") {
                    Some(i) => format!("{key}: {value}{}", &rest[rest[..i].trim_end().len()..]),
                    None => format!("{key}: {value}"),
                }
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        // A top-level key at column 0 is valid after any preceding block.
        out.push(format!("{key}: {value}"));
    }
    out.join("\n") + "\n"
}

/// Quote a string as a YAML scalar if it needs it.
pub fn yaml_scalar(s: &str) -> String {
    serde_yaml_ng::to_string(s)
        .map(|t| t.trim_end().to_string())
        .unwrap_or_else(|_| format!("{s:?}"))
}

/// A speed limit in bytes per second; `None` is unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rate(pub Option<NonZeroU32>);

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRate {
    Num(u64),
    Text(String),
}

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = match RawRate::deserialize(d)? {
            RawRate::Num(n) => n.to_string(),
            RawRate::Text(s) => s,
        };
        text.parse().map_err(serde::de::Error::custom)
    }
}

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

impl std::str::FromStr for Rate {
    type Err = String;

    /// `0`, `unlimited`, `none`, `off` or empty → unlimited; otherwise a number
    /// with a byte unit: `500KiB/s`, `2 MiB`, `1.5MiB/s`. K/M/G, KB/MB/GB and
    /// KiB/MiB/GiB all mean powers of 1024. Bit units (Mbps) are rejected so
    /// bits and bytes can't be confused.
    fn from_str(input: &str) -> Result<Self, String> {
        let s = input.trim().to_ascii_lowercase();
        if matches!(s.as_str(), "" | "0" | "unlimited" | "none" | "off") {
            return Ok(Rate(None));
        }
        let s = s.strip_suffix("/s").unwrap_or(&s).trim_end();
        if s.ends_with("bps") || s.ends_with("bit") || s.ends_with("bits") {
            return Err(format!(
                "'{input}': use bytes per second (e.g. 2MiB/s), not bits"
            ));
        }
        let split = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let unit = unit.trim();
        let mult = match unit {
            "b" => 1,
            "k" | "kb" | "kib" => KIB,
            "m" | "mb" | "mib" => MIB,
            "g" | "gb" | "gib" => GIB,
            "" => {
                return Err(format!(
                    "'{input}' needs a unit, e.g. 500KiB/s or 2MiB/s (use 0 for unlimited)"
                ));
            }
            _ => {
                return Err(format!(
                    "'{input}': unknown unit '{unit}' (use B, KiB, MiB or GiB)"
                ));
            }
        };
        let n: f64 = num
            .parse()
            .map_err(|_| format!("'{input}' is not a valid speed"))?;
        let bytes = (n * mult as f64).floor();
        if bytes < 1.0 {
            return Err(format!("'{input}' is below 1 byte/s (use 0 for unlimited)"));
        }
        if bytes > u32::MAX as f64 {
            return Err(format!(
                "'{input}' is too large (maximum is just under 4GiB/s)"
            ));
        }
        Ok(Rate(NonZeroU32::new(bytes as u32)))
    }
}

impl Rate {
    pub fn bps(self) -> Option<NonZeroU32> {
        self.0
    }

    /// Exact, re-parseable text: `unlimited` or e.g. `2MiB/s`.
    pub fn to_text(self) -> String {
        match self.0.map(NonZeroU32::get).map(u64::from) {
            None => "unlimited".into(),
            Some(b) if b % GIB == 0 => format!("{}GiB/s", b / GIB),
            Some(b) if b % MIB == 0 => format!("{}MiB/s", b / MIB),
            Some(b) if b % KIB == 0 => format!("{}KiB/s", b / KIB),
            Some(b) => format!("{b}B/s"),
        }
    }

    /// Raw YAML for writing back to the config file.
    pub fn to_yaml(self) -> String {
        match self.0 {
            None => "0".into(),
            Some(_) => self.to_text(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(s: &str) -> Option<u32> {
        s.parse::<Rate>().unwrap().0.map(NonZeroU32::get)
    }

    #[test]
    fn parses_rates() {
        assert_eq!(rate("0"), None);
        assert_eq!(rate("unlimited"), None);
        assert_eq!(rate(" "), None);
        assert_eq!(rate("500KiB/s"), Some(512_000));
        assert_eq!(rate("2MiB/s"), Some(2 * 1024 * 1024));
        assert_eq!(rate("2 mb"), Some(2 * 1024 * 1024));
        assert_eq!(rate("1.5M"), Some(1_572_864));
        assert_eq!(rate("100B/s"), Some(100));
        assert_eq!(rate("3GiB"), Some(3 * 1024 * 1024 * 1024));
    }

    #[test]
    fn rejects_bad_rates() {
        for bad in [
            "500",
            "fast",
            "5Mbps",
            "10 mbit",
            "2XB",
            "-1MiB",
            "4GiB",
            "0.0001KiB",
            ".",
        ] {
            assert!(bad.parse::<Rate>().is_err(), "{bad} should be rejected");
        }
        let msg = "500".parse::<Rate>().unwrap_err();
        assert!(msg.contains("unit"), "{msg}");
    }

    #[test]
    fn rate_text_round_trips() {
        for b in [
            1u32,
            100,
            1024,
            512_000,
            1_572_864,
            2 * 1024 * 1024,
            3 * 1024 * 1024 * 1024,
        ] {
            let r = Rate(NonZeroU32::new(b));
            assert_eq!(r.to_text().parse::<Rate>().unwrap(), r, "{}", r.to_text());
        }
        assert_eq!(Rate(None).to_text(), "unlimited");
        assert_eq!(Rate(None).to_yaml(), "0");
        assert_eq!(Rate(NonZeroU32::new(2 * 1024 * 1024)).to_text(), "2MiB/s");
    }

    #[test]
    fn default_template_equals_defaults() {
        let parsed = parse(DEFAULT_CONFIG).unwrap();
        let d = Config::default();
        assert_eq!(parsed.theme, d.theme);
        assert_eq!(parsed.max_download_speed, d.max_download_speed);
        assert_eq!(parsed.max_upload_speed, d.max_upload_speed);
        assert_eq!(parsed.peer_limit, d.peer_limit);
        assert_eq!(parsed.dht, d.dht);
        assert_eq!(parsed.ask_files, d.ask_files);
        assert_eq!(parsed.download_dir, d.download_dir);
        assert!(parsed.themes.is_empty());
    }

    #[test]
    fn parses_full_config() {
        let c = parse(
            "download_dir: ~/Torrents\ntheme: dracula\nmax_download_speed: 2MiB/s\n\
             max_upload_speed: 500KiB/s\npeer_limit: 50\ndht: false\nask_files: false\n\
             themes:\n  mine:\n    base: dracula\n    accent: '#ff0000'\n",
        )
        .unwrap();
        assert_eq!(c.theme, "dracula");
        assert_eq!(c.max_download_speed.bps().unwrap().get(), 2 * 1024 * 1024);
        assert_eq!(c.max_upload_speed.bps().unwrap().get(), 512_000);
        assert_eq!(c.peer_limit, 50);
        assert!(!c.dht && !c.ask_files);
        assert!(c.themes.contains_key("mine"));
        assert!(c.download_dir().unwrap().ends_with("Torrents"));
    }

    #[test]
    fn comment_only_and_empty_files_are_defaults() {
        assert_eq!(parse("").unwrap().theme, "default");
        assert_eq!(parse("# nothing\n\n").unwrap().theme, "default");
    }

    #[test]
    fn typos_and_bad_values_are_errors_with_location() {
        let e = parse("them: dracula\n").unwrap_err().to_string();
        assert!(e.contains("them"), "{e}");
        let e = parse("dht: maybe\n").unwrap_err().to_string();
        assert!(e.contains("line 1"), "{e}");
        let e = parse("max_upload_speed: 500\n").unwrap_err().to_string();
        assert!(e.contains("unit"), "{e}");
        assert!(parse("peer_limit: -3\n").is_err());
    }

    #[test]
    fn relative_download_dir_is_rejected() {
        let c = parse("download_dir: downloads\n").unwrap();
        assert!(c.download_dir().is_err());
        let c = parse("download_dir: /srv/torrents\n").unwrap();
        assert_eq!(c.download_dir().unwrap(), PathBuf::from("/srv/torrents"));
    }

    #[test]
    fn replace_key_keeps_everything_else() {
        let src =
            "# top\ntheme: default   # pick one\nmax_upload_speed: 0\nthemes:\n  a:\n    base: x\n";
        let out = replace_key(src, "theme", "dracula");
        assert_eq!(
            out,
            "# top\ntheme: dracula   # pick one\nmax_upload_speed: 0\nthemes:\n  a:\n    base: x\n"
        );
        // not confused by a prefix match like `themes:`
        let out = replace_key("themes:\n  a: {}\n", "theme", "x");
        assert!(out.starts_with("themes:") && out.ends_with("theme: x\n"));
        // commented-out template lines are not treated as the key
        let out = replace_key("# theme: foo\n", "theme", "x");
        assert_eq!(out, "# theme: foo\ntheme: x\n");
    }

    #[test]
    fn set_value_edits_file_and_creates_missing() {
        let dir = tempdir("set_value");
        let path = dir.join("nested/config.yml");
        set_value(&path, "theme", &yaml_scalar("tokyo-night")).unwrap();
        let c = load(&path).unwrap();
        assert_eq!(c.theme, "tokyo-night");
        // comments from the template survive
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("# torrentui configuration")
        );

        set_value(
            &path,
            "max_download_speed",
            &Rate(NonZeroU32::new(1024)).to_yaml(),
        )
        .unwrap();
        let c = load(&path).unwrap();
        assert_eq!(c.max_download_speed.bps().unwrap().get(), 1024);
        assert_eq!(c.theme, "tokyo-night");

        // an invalid edit is refused and the file is left alone
        let before = fs::read_to_string(&path).unwrap();
        assert!(set_value(&path, "dht", "perhaps").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn set_value_follows_symlinked_config() {
        let dir = tempdir("symlink");
        let real = dir.join("dots/config.yml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "theme: default\n").unwrap();
        let link = dir.join("config.yml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        set_value(&link, "theme", "dracula").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "theme: dracula\n");
    }

    #[test]
    fn load_creates_default_file() {
        let dir = tempdir("load");
        let path = dir.join("torrentui/config.yml");
        let c = load(&path).unwrap();
        assert_eq!(c.theme, "default");
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
    }

    #[test]
    fn migrates_legacy_sessions_once() {
        let dir = tempdir("migrate");
        let legacy = dir.join("com.rqbit.session");
        let new = dir.join("torrentui/sessions");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("session.json"), "{}").unwrap();
        fs::write(legacy.join("abc.torrent"), "x").unwrap();

        assert!(migrate_sessions(&legacy, &new).unwrap());
        assert!(!legacy.exists());
        assert_eq!(fs::read_to_string(new.join("session.json")).unwrap(), "{}");
        assert!(new.join("abc.torrent").exists());

        // second run: nothing left to move
        assert!(!migrate_sessions(&legacy, &new).unwrap());
    }

    #[test]
    fn migration_never_overwrites_or_merges() {
        let dir = tempdir("nomerge");
        let legacy = dir.join("old");
        let new = dir.join("new");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("session.json"), "old").unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join("session.json"), "new").unwrap();
        assert!(!migrate_sessions(&legacy, &new).unwrap());
        assert_eq!(fs::read_to_string(new.join("session.json")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(legacy.join("session.json")).unwrap(),
            "old"
        );

        // no legacy dir at all
        assert!(!migrate_sessions(&dir.join("missing"), &dir.join("fresh")).unwrap());
        assert!(!dir.join("fresh").exists());
    }

    #[test]
    fn yaml_scalar_quotes_when_needed() {
        assert_eq!(yaml_scalar("dracula"), "dracula");
        let q = yaml_scalar("my: theme");
        assert_eq!(serde_yaml_ng::from_str::<String>(&q).unwrap(), "my: theme");
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("torrentui-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn on_complete_parses_and_defaults() {
        let cfg = parse(
            "on_complete:\n  - name: sort\n    run: [\"~/bin/sort.sh\", \"{path}\"]\n    tags: [movies]\n  - run: [notify-send, done]\n    timeout_secs: 0\n",
        )
        .unwrap();
        assert_eq!(cfg.on_complete.len(), 2);
        assert_eq!(cfg.on_complete[0].timeout_secs, 600);
        assert_eq!(cfg.on_complete[1].timeout_secs, 0);
        assert!(cfg.on_complete[1].tags.is_empty());
        assert!(crate::actions::resolve(&cfg.on_complete).is_ok());
        assert!(parse("").unwrap().on_complete.is_empty());
    }

    #[test]
    fn on_complete_typos_and_missing_run_are_errors() {
        assert!(parse("on_complete:\n  - run: [a]\n    tag: [x]\n").is_err());
        assert!(parse("on_complete:\n  - name: no-run\n").is_err());
        // A shell string is rejected on purpose: `run` is an argument list.
        assert!(parse("on_complete:\n  - run: \"echo hi\"\n").is_err());
    }

    #[test]
    fn the_commented_example_in_the_template_is_valid_when_uncommented() {
        let uncommented: String = DEFAULT_CONFIG
            .lines()
            .skip_while(|l| !l.starts_with("# on_complete:"))
            .take_while(|l| !l.is_empty())
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix('#'))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = parse(&uncommented).unwrap();
        assert_eq!(cfg.on_complete.len(), 1);
        assert!(crate::actions::resolve(&cfg.on_complete).is_ok());
    }

    #[test]
    fn set_value_keeps_on_complete_intact() {
        let dir = std::env::temp_dir().join(format!("torrentui-cfg-oc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.yml");
        fs::create_dir_all(&dir).unwrap();
        let text = "theme: default\non_complete:\n  - run: [a, \"{path}\"]\n    tags: [x]\n";
        fs::write(&path, text).unwrap();
        set_value(&path, "theme", "dracula").unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, text.replace("theme: default", "theme: dracula"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_download_dir_is_always_absolute() {
        let d = Config::default().download_dir().unwrap();
        assert!(d.is_absolute(), "{d:?}");
    }
}
