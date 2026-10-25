//! macOS integration: the magnet-link handler app, notifications, and the
//! launchd agent.
//!
//! The functions that generate files and commands are pure and compiled on
//! every platform, so their output is unit-tested everywhere; only the code
//! that actually runs `osacompile`, `launchctl` and friends is meant for macOS
//! (the dispatch in `desktop.rs` never calls it elsewhere).
//!
//! A custom URL scheme on macOS is handled by an app bundle that receives the
//! URL as an Apple Event, not as a command-line argument. So `register` builds
//! a tiny AppleScript applet whose only job is to call `torrentui open`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};

pub const BUNDLE_ID: &str = "com.torrentui.opener";
pub const AGENT_LABEL: &str = "com.torrentui.daemon";
const APP_NAME: &str = "torrentui.app";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
                          LaunchServices.framework/Support/lsregister";
const PLISTBUDDY: &str = "/usr/libexec/PlistBuddy";

/// `Terminal.app`, which every Mac has. `open -a Terminal <file>` runs an
/// executable in a new window.
pub fn default_terminal() -> Vec<String> {
    vec!["open".into(), "-a".into(), "Terminal".into()]
}

/// Quote for a POSIX shell: `it's` -> `'it'\''s'`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// An AppleScript string literal. Control characters are refused rather than
/// escaped: nothing legitimate (an executable path, a terminal command)
/// contains them.
fn applescript_string(s: &str) -> Result<String> {
    if let Some(c) = s.chars().find(|c| c.is_control()) {
        bail!("{s:?} contains the control character {c:?}");
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The applet's source. It receives magnet links (`open location`) and
/// `.torrent` files (`open`) and passes them to `torrentui open`, quoting
/// every value for the shell.
pub fn handler_script(exe: &str, ui: bool, terminal: Option<&[String]>) -> Result<String> {
    let mut base = format!("{} open", sh_quote(exe));
    if ui {
        base.push_str(" --ui");
        if let Some(t) = terminal {
            base.push_str(&format!(" --terminal {}", sh_quote(&t.join(" "))));
        }
    }
    let base = applescript_string(&base)?;
    Ok(format!(
        "property baseCmd : {base}\n\
         \n\
         on open location theURL\n\
         \ttry\n\
         \t\tdo shell script baseCmd & \" -- \" & quoted form of theURL\n\
         \tend try\n\
         end open location\n\
         \n\
         on open theFiles\n\
         \tset args to \"\"\n\
         \trepeat with f in theFiles\n\
         \t\tset args to args & \" \" & quoted form of POSIX path of f\n\
         \tend repeat\n\
         \ttry\n\
         \t\tdo shell script baseCmd & \" --\" & args\n\
         \tend try\n\
         end open\n\
         \n\
         on run\n\
         \t-- Opened directly: nothing to add.\n\
         end run\n"
    ))
}

/// `PlistBuddy -c` commands that make the applet claim `magnet:` links and
/// `.torrent` files and keep it out of the Dock. Entries starting with `-`
/// may fail harmlessly (deleting a key that isn't there).
pub fn plist_commands() -> Vec<String> {
    let mut c: Vec<String> = Vec::new();
    for (key, ty, value) in [
        ("CFBundleIdentifier", "string", BUNDLE_ID),
        ("CFBundleName", "string", "torrentui"),
        ("LSUIElement", "bool", "true"),
    ] {
        c.push(format!("-Delete :{key}"));
        c.push(format!("Add :{key} {ty} {value}"));
    }
    for cmd in [
        "-Delete :CFBundleURLTypes",
        "Add :CFBundleURLTypes array",
        "Add :CFBundleURLTypes:0 dict",
        "Add :CFBundleURLTypes:0:CFBundleURLName string \"BitTorrent magnet link\"",
        "Add :CFBundleURLTypes:0:CFBundleURLSchemes array",
        "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string magnet",
        "-Delete :CFBundleDocumentTypes",
        "Add :CFBundleDocumentTypes array",
        "Add :CFBundleDocumentTypes:0 dict",
        "Add :CFBundleDocumentTypes:0:CFBundleTypeName string \"BitTorrent file\"",
        "Add :CFBundleDocumentTypes:0:CFBundleTypeRole string Viewer",
        "Add :CFBundleDocumentTypes:0:CFBundleTypeExtensions array",
        "Add :CFBundleDocumentTypes:0:CFBundleTypeExtensions:0 string torrent",
        "Add :CFBundleDocumentTypes:0:LSItemContentTypes array",
        "Add :CFBundleDocumentTypes:0:LSItemContentTypes:0 string org.bittorrent.torrent",
    ] {
        c.push(cmd.to_string());
    }
    c
}

/// The launchd agent that keeps the daemon running. It restarts the daemon
/// only if it exits with an error, so `torrentui daemon stop` sticks.
pub fn launch_agent_plist(exe: &str, log: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{AGENT_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{}</string>\n\t\t<string>daemon</string>\n\t\t<string>run</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
         \t<key>ProcessType</key>\n\t<string>Background</string>\n\
         \t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\
         \t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>\n\t</dict>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
         </dict>\n</plist>\n",
        xml_escape(exe),
        xml_escape(log)
    )
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| eyre!("cannot determine your home directory"))
}

fn app_path() -> Result<PathBuf> {
    Ok(home()?.join("Applications").join(APP_NAME))
}

fn agent_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist")))
}

/// Run a command, returning its stdout; a failure carries its stderr.
fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .wrap_err_with(|| format!("cannot run {cmd}"))?;
    if !out.status.success() {
        bail!(
            "{cmd} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn have(cmd: &str) -> bool {
    std::process::Command::new("/usr/bin/which")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Build `~/Applications/torrentui.app` and make it the link handler.
pub fn register(exe: &str, ui: bool, terminal: Option<&[String]>) -> Result<Vec<String>> {
    let script = handler_script(exe, ui, terminal)?;
    let app = app_path()?;
    let dir = app.parent().expect("has parent");
    fs::create_dir_all(dir).wrap_err_with(|| format!("cannot create {}", dir.display()))?;

    let src = std::env::temp_dir().join(format!(
        "torrentui-opener-{}.applescript",
        std::process::id()
    ));
    fs::write(&src, script)?;
    let _ = fs::remove_dir_all(&app);
    let compiled = run(
        "/usr/bin/osacompile",
        &["-o", &app.to_string_lossy(), &src.to_string_lossy()],
    );
    let _ = fs::remove_file(&src);
    compiled?;

    let plist = app.join("Contents").join("Info.plist");
    for cmd in plist_commands() {
        let result = run(
            PLISTBUDDY,
            &["-c", cmd.trim_start_matches('-'), &plist.to_string_lossy()],
        );
        // "-Delete" lines are allowed to fail: the key may not exist yet.
        if !cmd.starts_with('-') {
            result.wrap_err_with(|| format!("PlistBuddy: {cmd}"))?;
        }
    }
    // Editing Info.plist invalidated the applet's signature, and Apple
    // Silicon refuses to launch a bundle whose signature doesn't match.
    run(
        "/usr/bin/codesign",
        &["--force", "--deep", "--sign", "-", &app.to_string_lossy()],
    )?;
    run(LSREGISTER, &["-f", &app.to_string_lossy()])?;

    let mut done = vec![format!("Built {}", app.display())];
    if have("duti") {
        let schemes = [
            vec!["-s", BUNDLE_ID, "magnet"],
            vec!["-s", BUNDLE_ID, ".torrent", "all"],
        ];
        for args in schemes {
            match run("duti", &args) {
                Ok(_) => done.push(format!(
                    "Set the default with duti: {}",
                    args[2..].join(" ")
                )),
                Err(e) => done.push(format!("duti could not set {}: {e:#}", args[2])),
            }
        }
    } else {
        done.push(
            "macOS uses torrentui for magnet links automatically if no other app claims them. \
             If another torrent client is installed, set the default with \
             `brew install duti`, then re-run `torrentui register`"
                .into(),
        );
        done.push(
            "For .torrent files: Finder > Get Info on one > Open with > torrentui > Change All…"
                .into(),
        );
    }
    done.push(if ui {
        "A link opens a terminal with torrentui if no window is attached".into()
    } else {
        "Links are added in the background (a notification confirms them)".into()
    });
    Ok(done)
}

pub fn unregister() -> Result<Vec<String>> {
    let app = app_path()?;
    if !app.exists() {
        return Ok(vec![
            "Nothing to remove: torrentui was not registered".into(),
        ]);
    }
    let _ = run(LSREGISTER, &["-u", &app.to_string_lossy()]);
    fs::remove_dir_all(&app).wrap_err_with(|| format!("cannot remove {}", app.display()))?;
    Ok(vec![
        format!("Removed {}", app.display()),
        "macOS falls back to another handler for magnet links".into(),
    ])
}

fn uid() -> Result<String> {
    Ok(run("/usr/bin/id", &["-u"])?.trim().to_string())
}

pub fn install_service(exe: &str, log: &Path) -> Result<Vec<String>> {
    let path = agent_path()?;
    fs::create_dir_all(path.parent().expect("has parent"))?;
    fs::write(&path, launch_agent_plist(exe, &log.to_string_lossy()))?;
    Ok(vec![
        format!("Wrote {}", path.display()),
        "Start it now and at every login with:".into(),
        format!("  launchctl bootstrap gui/$(id -u) {}", path.display()),
        "(stop a manually started daemon first: torrentui daemon stop)".into(),
    ])
}

pub fn uninstall_service() -> Result<Vec<String>> {
    let path = agent_path()?;
    if !path.exists() {
        return Ok(vec![
            "Nothing to remove: the service was not installed".into(),
        ]);
    }
    // Stops it and unloads it before the file disappears.
    if let Ok(uid) = uid() {
        let _ = run(
            "/bin/launchctl",
            &["bootout", &format!("gui/{uid}/{AGENT_LABEL}")],
        );
    }
    fs::remove_file(&path)?;
    Ok(vec![format!("Removed {}", path.display())])
}

/// A notification via `osascript`. The text travels in environment variables
/// that the script reads, so a torrent name can never be parsed as AppleScript.
pub fn notify(summary: &str, body: &str) {
    let clean = |s: &str| s.replace('\0', "");
    let mut cmd = tokio::process::Command::new("/usr/bin/osascript");
    cmd.args([
        "-e",
        "display notification (system attribute \"TORRENTUI_BODY\") \
         with title (system attribute \"TORRENTUI_TITLE\")",
    ])
    .env("TORRENTUI_BODY", clean(body))
    .env("TORRENTUI_TITLE", clean(summary))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_and_applescript_quoting() {
        assert_eq!(sh_quote("/a b/it's"), "'/a b/it'\\''s'");
        assert_eq!(applescript_string("a\"b\\c").unwrap(), "\"a\\\"b\\\\c\"");
        assert!(applescript_string("a\nb").is_err());
    }

    #[test]
    fn handler_script_passes_urls_and_files_to_open() {
        let s = handler_script("/Users/me/.cargo/bin/torrentui", false, None).unwrap();
        assert!(
            s.contains("property baseCmd : \"'/Users/me/.cargo/bin/torrentui' open\""),
            "{s}"
        );
        assert!(s.contains("on open location theURL"));
        assert!(s.contains("do shell script baseCmd & \" -- \" & quoted form of theURL"));
        assert!(s.contains("quoted form of POSIX path of f"));
        assert!(s.contains("on open theFiles"));
        // Failures must not surface as an AppleScript error dialog.
        assert_eq!(s.matches("\ttry\n").count(), 2);
        assert_eq!(s.matches("end try").count(), 2);
    }

    #[test]
    fn handler_script_with_ui_and_hostile_paths() {
        let t = vec!["open".to_string(), "-a".to_string(), "iTerm".to_string()];
        let s = handler_script("/Users/o'neil/My Apps/torrentui", true, Some(&t)).unwrap();
        // Shell-quoted first, then escaped for the AppleScript literal.
        assert!(
            s.contains(
                "'/Users/o'\\\\''neil/My Apps/torrentui' open --ui --terminal 'open -a iTerm'"
            ),
            "{s}"
        );
        assert!(handler_script("/bad\npath", false, None).is_err());
    }

    #[test]
    fn plist_commands_claim_magnet_and_torrent() {
        let c = plist_commands();
        assert!(
            c.iter()
                .any(|x| x == "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string magnet")
        );
        assert!(
            c.iter()
                .any(|x| x.contains("CFBundleTypeExtensions:0 string torrent"))
        );
        assert!(c.iter().any(|x| x == "Add :LSUIElement bool true"));
        assert!(
            c.iter()
                .any(|x| x == &format!("Add :CFBundleIdentifier string {BUNDLE_ID}"))
        );
        // Every container is created before its children.
        let pos = |needle: &str| c.iter().position(|x| x == needle).unwrap();
        assert!(pos("Add :CFBundleURLTypes array") < pos("Add :CFBundleURLTypes:0 dict"));
        assert!(
            pos("Add :CFBundleURLTypes:0:CFBundleURLSchemes array")
                < pos("Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string magnet")
        );
        // Only deletes are allowed to fail.
        assert!(
            c.iter()
                .filter(|x| x.starts_with('-'))
                .all(|x| x.starts_with("-Delete "))
        );
    }

    #[test]
    fn launch_agent_is_valid_looking_xml_with_escaped_values() {
        let p = launch_agent_plist("/Users/a&b/torrentui", "/Users/a&b/log<1>.log");
        assert!(p.contains("<string>/Users/a&amp;b/torrentui</string>"));
        assert!(p.contains("log&lt;1&gt;.log"));
        assert!(p.contains("<string>com.torrentui.daemon</string>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(p.contains("<string>daemon</string>") && p.contains("<string>run</string>"));
        assert!(p.starts_with("<?xml") && p.trim_end().ends_with("</plist>"));
        assert_eq!(p.matches("<dict>").count(), p.matches("</dict>").count());
        assert_eq!(p.matches("<array>").count(), p.matches("</array>").count());
    }

    #[test]
    fn system_tool_paths_are_well_formed() {
        assert!(LSREGISTER.starts_with("/System/Library/Frameworks/CoreServices.framework/"));
        assert!(LSREGISTER.ends_with("/Support/lsregister"));
        assert!(!LSREGISTER.contains(char::is_whitespace), "{LSREGISTER}");
    }

    #[test]
    fn names_with_spaces_are_quoted_for_plistbuddy() {
        let c = plist_commands();
        assert!(
            c.iter()
                .any(|x| x.ends_with("CFBundleURLName string \"BitTorrent magnet link\""))
        );
        assert!(
            c.iter()
                .any(|x| x.ends_with("CFBundleTypeName string \"BitTorrent file\""))
        );
    }

    #[test]
    fn default_terminal_runs_a_file_in_terminal_app() {
        assert_eq!(default_terminal(), ["open", "-a", "Terminal"]);
    }
}
