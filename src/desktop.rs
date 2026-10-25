//! Desktop integration: notifications, magnet-link registration, and the
//! optional systemd user service. Everything here shells out to standard
//! tools with explicit argument lists; nothing goes through a shell.

use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Stdio,
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};

/// Escape the characters a notification daemon would read as markup.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Show a desktop notification via `notify-send`, if it is installed.
/// Best effort and non-blocking; needs a Tokio runtime.
pub fn notify(summary: &str, body: &str) {
    let mut cmd = tokio::process::Command::new("notify-send");
    cmd.args(["--app-name=torrentui", "--"])
        .arg(summary)
        .arg(escape_markup(body))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        // Reap it so a long-lived daemon doesn't accumulate zombies.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

const HANDLER_FILE: &str = "torrentui-open.desktop";
const LAUNCHER_FILE: &str = "torrentui.desktop";
const SERVICE_FILE: &str = "torrentui.service";
const MIME_TYPES: [&str; 2] = ["x-scheme-handler/magnet", "application/x-bittorrent"];

/// Terminal emulators and the arguments that make them run a command.
const TERMINALS: &[(&str, &[&str])] = &[
    ("ghostty", &["-e"]),
    ("kitty", &[]),
    ("alacritty", &["-e"]),
    ("foot", &[]),
    ("wezterm", &["start", "--"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xterm", &["-e"]),
];

fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// A terminal emulator command prefix: `$TERMINAL` if set and installed,
/// else the first known one found on `PATH`.
pub fn detect_terminal() -> Option<Vec<String>> {
    let args_for = |name: &str| -> Vec<String> {
        TERMINALS
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(vec!["-e".to_string()], |(_, a)| {
                a.iter().map(|s| s.to_string()).collect()
            })
    };
    if let Some(t) = std::env::var_os("TERMINAL") {
        let t = t.to_string_lossy().into_owned();
        let name = Path::new(&t).file_name()?.to_string_lossy().into_owned();
        if find_in_path(&t).is_some() || (t.starts_with('/') && Path::new(&t).exists()) {
            let mut cmd = vec![t];
            cmd.extend(args_for(&name));
            return Some(cmd);
        }
    }
    TERMINALS.iter().find_map(|(name, args)| {
        find_in_path(name).map(|_| {
            let mut cmd = vec![name.to_string()];
            cmd.extend(args.iter().map(|s| s.to_string()));
            cmd
        })
    })
}

/// Split `--terminal "ghostty -e"` into a command prefix.
pub fn parse_terminal(text: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = text.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        bail!("--terminal needs a command, e.g. \"ghostty -e\"");
    }
    Ok(parts)
}

/// Open the TUI in a new terminal window.
pub fn launch_terminal(prefix: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let (cmd, args) = prefix
        .split_first()
        .ok_or_else(|| eyre!("no terminal command"))?;
    std::process::Command::new(cmd)
        .args(args)
        .arg(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .wrap_err_with(|| format!("cannot start the terminal '{cmd}'"))?;
    Ok(())
}

/// Quote one argument for an `Exec=` line (or a systemd `ExecStart=`).
/// Characters that would need multi-level escaping are refused outright
/// rather than risk writing a line that means something else.
fn exec_arg(arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("\"\"".into());
    }
    if let Some(bad) = arg
        .chars()
        .find(|c| matches!(c, '"' | '`' | '$' | '\\' | '%') || c.is_control())
    {
        bail!("'{arg}' contains {bad:?}, which can't be used in a desktop entry");
    }
    let reserved = " \t'><~|&;*?#()";
    if arg.chars().any(|c| reserved.contains(c)) {
        Ok(format!("\"{arg}\""))
    } else {
        Ok(arg.to_string())
    }
}

fn exec_line(parts: &[&str]) -> Result<String> {
    Ok(parts
        .iter()
        .map(|p| exec_arg(p))
        .collect::<Result<Vec<_>>>()?
        .join(" "))
}

fn handler_entry(exe: &str, ui: bool, terminal: Option<&[String]>) -> Result<String> {
    let mut parts = vec![exe, "open"];
    let joined;
    if ui {
        parts.push("--ui");
        if let Some(t) = terminal {
            joined = t.join(" ");
            parts.push("--terminal");
            parts.push(&joined);
        }
    }
    parts.push("%U");
    // `%U` is the one field code and must stay unquoted.
    let (last, rest) = parts.split_last().expect("non-empty");
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=torrentui (add torrent)\n\
         Comment=Add magnet links and .torrent files to torrentui\n\
         Exec={} {last}\nNoDisplay=true\nTerminal=false\n\
         MimeType=x-scheme-handler/magnet;application/x-bittorrent;\n\
         Categories=Network;FileTransfer;\n",
        exec_line(rest)?
    ))
}

fn launcher_entry(exe: &str, terminal: &[String]) -> Result<String> {
    let mut parts: Vec<&str> = terminal.iter().map(String::as_str).collect();
    parts.push(exe);
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=torrentui\n\
         Comment=Terminal BitTorrent client\nExec={}\nTerminal=false\n\
         Categories=Network;FileTransfer;\n",
        exec_line(&parts)?
    ))
}

fn applications_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| eyre!("cannot determine your data directory"))?
        .join("applications"))
}

fn exe_str() -> Result<String> {
    Ok(std::env::current_exe()
        .wrap_err("cannot find the torrentui executable")?
        .to_str()
        .ok_or_else(|| eyre!("the executable path is not valid UTF-8"))?
        .to_string())
}

fn current_default(mime: &str) -> Option<String> {
    let out = std::process::Command::new("xdg-mime")
        .args(["query", "default", mime])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn refresh_desktop_database(dir: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub struct RegisterOpts {
    /// When a link arrives and no window is open, open one.
    pub ui: bool,
    pub terminal: Option<Vec<String>>,
    /// Replace another application as the default handler.
    pub force: bool,
}

/// Make torrentui the handler for magnet links and .torrent files.
/// Returns lines describing what was done.
pub fn register(opts: &RegisterOpts) -> Result<Vec<String>> {
    if find_in_path("xdg-mime").is_none() {
        bail!(
            "xdg-mime was not found (install xdg-utils). By hand: install the .desktop \
             file from `torrentui register` into ~/.local/share/applications and set it as \
             the default for x-scheme-handler/magnet"
        );
    }
    let exe = exe_str()?;
    let terminal = opts.terminal.clone().or_else(detect_terminal);
    if opts.ui && terminal.is_none() {
        bail!("--ui needs a terminal emulator and none was found; pass --terminal \"CMD ARGS\"");
    }

    if !opts.force {
        for mime in MIME_TYPES {
            if let Some(other) = current_default(mime)
                && other != HANDLER_FILE
            {
                bail!(
                    "{mime} is currently handled by {other}; run `torrentui register --force` \
                     to replace it"
                );
            }
        }
    }

    let dir = applications_dir()?;
    fs::create_dir_all(&dir).wrap_err_with(|| format!("cannot create {}", dir.display()))?;
    let handler = dir.join(HANDLER_FILE);
    fs::write(&handler, handler_entry(&exe, opts.ui, terminal.as_deref())?)?;
    let mut done = vec![format!("Wrote {}", handler.display())];
    if let Some(t) = &terminal {
        let launcher = dir.join(LAUNCHER_FILE);
        fs::write(&launcher, launcher_entry(&exe, t)?)?;
        done.push(format!("Wrote {}", launcher.display()));
    }
    refresh_desktop_database(&dir);

    let status = std::process::Command::new("xdg-mime")
        .args(["default", HANDLER_FILE])
        .args(MIME_TYPES)
        .stdin(Stdio::null())
        .status()
        .wrap_err("cannot run xdg-mime")?;
    if !status.success() {
        bail!("xdg-mime failed ({status})");
    }
    for mime in MIME_TYPES {
        if current_default(mime).as_deref() != Some(HANDLER_FILE) {
            bail!("{mime} is still not handled by torrentui; check ~/.config/mimeapps.list");
        }
    }
    done.push(format!(
        "Registered torrentui for {}",
        MIME_TYPES.join(" and ")
    ));
    done.push(if opts.ui {
        "A link opens a terminal with torrentui if no window is attached".into()
    } else {
        "Links are added in the background (a notification confirms them)".into()
    });
    done.push(
        "Browsers may ask once which application to use; choose torrentui (add torrent)".into(),
    );
    Ok(done)
}

/// Remove the desktop entries. `xdg-mime` has no way to clear a default, but
/// one that names a missing file is ignored.
pub fn unregister() -> Result<Vec<String>> {
    let dir = applications_dir()?;
    let mut done = Vec::new();
    for file in [HANDLER_FILE, LAUNCHER_FILE] {
        let path = dir.join(file);
        match fs::remove_file(&path) {
            Ok(()) => done.push(format!("Removed {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).wrap_err_with(|| format!("cannot remove {}", path.display())),
        }
    }
    refresh_desktop_database(&dir);
    if done.is_empty() {
        done.push("Nothing to remove: torrentui was not registered".into());
    } else {
        done.push(
            "Applications that named torrentui as the default fall back to the next handler".into(),
        );
    }
    Ok(done)
}

fn service_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| eyre!("cannot determine your config directory"))?
        .join("systemd")
        .join("user")
        .join(SERVICE_FILE))
}

fn service_unit(exe: &str) -> Result<String> {
    Ok(format!(
        "[Unit]\nDescription=torrentui BitTorrent daemon\nAfter=network-online.target\n\n\
         [Service]\nType=simple\nExecStart={}\nRestart=on-failure\n\
         # Let post-download actions finish if the daemon is stopped.\nKillMode=process\n\
         TimeoutStopSec=60\n\n[Install]\nWantedBy=default.target\n",
        exec_line(&[exe, "daemon", "run"])?
    ))
}

fn systemctl(args: &[&str]) -> bool {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn install_service() -> Result<Vec<String>> {
    let path = service_path()?;
    fs::create_dir_all(path.parent().expect("has parent"))?;
    fs::write(&path, service_unit(&exe_str()?)?)?;
    let reloaded = systemctl(&["daemon-reload"]);
    let mut done = vec![format!("Wrote {}", path.display())];
    if !reloaded {
        done.push("systemctl --user daemon-reload failed; run it yourself".into());
    }
    done.push("Start it now and on every login with:".into());
    done.push("  systemctl --user enable --now torrentui".into());
    done.push("(stop a manually started daemon first: torrentui daemon stop)".into());
    Ok(done)
}

pub fn uninstall_service() -> Result<Vec<String>> {
    let path = service_path()?;
    let mut done = Vec::new();
    if path.exists() {
        // Stops it and removes the login symlink before the unit disappears.
        systemctl(&["disable", "--now", SERVICE_FILE]);
        fs::remove_file(&path)?;
        systemctl(&["daemon-reload"]);
        done.push(format!("Removed {}", path.display()));
    } else {
        done.push("Nothing to remove: the service was not installed".into());
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_in_torrent_names_is_escaped() {
        assert_eq!(
            escape_markup("a <b>x</b> & c"),
            "a &lt;b&gt;x&lt;/b&gt; &amp; c"
        );
    }

    #[test]
    fn exec_args_are_quoted_or_refused() {
        assert_eq!(
            exec_arg("/usr/bin/torrentui").unwrap(),
            "/usr/bin/torrentui"
        );
        assert_eq!(
            exec_arg("/home/a b/torrentui").unwrap(),
            "\"/home/a b/torrentui\""
        );
        assert_eq!(exec_arg("ghostty -e").unwrap(), "\"ghostty -e\"");
        assert_eq!(exec_arg("").unwrap(), "\"\"");
        for bad in ["a\"b", "a$b", "a`b", "a\\b", "100%", "a\nb"] {
            assert!(exec_arg(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn handler_entry_has_the_right_shape() {
        let e = handler_entry("/opt/my apps/torrentui", false, None).unwrap();
        assert!(
            e.contains("Exec=\"/opt/my apps/torrentui\" open %U\n"),
            "{e}"
        );
        assert!(e.contains("MimeType=x-scheme-handler/magnet;application/x-bittorrent;"));
        assert!(e.contains("NoDisplay=true") && e.contains("Terminal=false"));

        let t = vec!["ghostty".to_string(), "-e".to_string()];
        let e = handler_entry("/usr/bin/torrentui", true, Some(&t)).unwrap();
        assert!(
            e.contains("Exec=/usr/bin/torrentui open --ui --terminal \"ghostty -e\" %U\n"),
            "{e}"
        );
        assert!(handler_entry("/bad$path/torrentui", false, None).is_err());
    }

    #[test]
    fn launcher_and_service_entries() {
        let t = vec!["ghostty".to_string(), "-e".to_string()];
        let l = launcher_entry("/usr/bin/torrentui", &t).unwrap();
        assert!(l.contains("Exec=ghostty -e /usr/bin/torrentui\n") && l.contains("Name=torrentui"));
        let u = service_unit("/usr/bin/torrentui").unwrap();
        assert!(u.contains("ExecStart=/usr/bin/torrentui daemon run\n"));
        assert!(u.contains("WantedBy=default.target") && u.contains("KillMode=process"));
    }

    #[test]
    fn terminal_text_is_split() {
        assert_eq!(parse_terminal(" ghostty  -e ").unwrap(), ["ghostty", "-e"]);
        assert!(parse_terminal("  ").is_err());
    }
}
