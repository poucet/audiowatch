//! The LaunchAgent that starts the watcher at login.
//!
//! `--install-agent` writes the plist and prints the command to load it. It
//! deliberately does not load it: starting a background daemon is the sort of
//! thing a person should type themselves.

use crate::config;
use std::path::{Path, PathBuf};

pub const LABEL: &str = "local.audiowatch";

pub fn plist_path() -> PathBuf {
    config::home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn stdio_dir() -> PathBuf {
    config::home().join("Library/Logs/audiowatch")
}

/// Escape text for an XML text node.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The plist contents for a given `audiowatch` binary and config file.
pub fn plist(binary: &Path, config_path: &Path) -> String {
    let args = [
        binary.to_string_lossy().to_string(),
        "--config".to_string(),
        config_path.to_string_lossy().to_string(),
        "--quiet".to_string(),
    ];
    let args_xml = args
        .iter()
        .map(|a| format!("\t\t<string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("\n");
    let out = stdio_dir().join("agent.out.log");
    let err = stdio_dir().join("agent.err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{args_xml}
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{out}</string>
	<key>StandardErrorPath</key>
	<string>{err}</string>
</dict>
</plist>
"#,
        label = LABEL,
        args_xml = args_xml,
        out = xml_escape(&out.to_string_lossy()),
        err = xml_escape(&err.to_string_lossy()),
    )
}

/// Write the plist and return its path plus the commands to run.
pub fn install(binary: &Path, config_path: &Path) -> Result<(PathBuf, String), String> {
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(stdio_dir())
        .map_err(|e| format!("cannot create {}: {e}", stdio_dir().display()))?;
    std::fs::write(&path, plist(binary, config_path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((path.clone(), commands(&path)))
}

/// The commands a person runs by hand, once.
pub fn commands(plist: &Path) -> String {
    let p = plist.display();
    format!(
        "  # start it now and at every login:\n\
         \x20 launchctl bootstrap gui/$(id -u) {p}\n\n\
         \x20 # after editing the config, restart it:\n\
         \x20 launchctl kickstart -k gui/$(id -u)/{LABEL}\n\n\
         \x20 # stop it and remove it from login:\n\
         \x20 launchctl bootout gui/$(id -u)/{LABEL}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_names_the_binary_the_config_and_runs_at_login() {
        let p = plist(
            Path::new("/usr/local/bin/audiowatch"),
            Path::new("/tmp/c.conf"),
        );
        assert!(p.contains("<string>local.audiowatch</string>"));
        assert!(p.contains("<string>/usr/local/bin/audiowatch</string>"));
        assert!(p.contains("<string>/tmp/c.conf</string>"));
        assert!(p.contains("<string>--config</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(p.contains("agent.err.log"));
    }

    #[test]
    fn the_plist_is_well_formed_enough_for_plutil() {
        let dir = std::env::temp_dir().join(format!("audiowatch-plist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.plist");
        std::fs::write(
            &f,
            plist(
                Path::new("/usr/local/bin/audiowatch"),
                Path::new("/tmp/c.conf"),
            ),
        )
        .unwrap();
        let out = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&f)
            .output()
            .expect("plutil should exist on macOS");
        assert!(
            out.status.success(),
            "plutil rejected the plist: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_with_xml_metacharacters_cannot_break_the_plist() {
        let nasty = PathBuf::from("/tmp/a&b<c>\"d\".app/audiowatch");
        let p = plist(&nasty, Path::new("/tmp/c.conf"));
        assert!(
            p.contains("/tmp/a&amp;b&lt;c&gt;&quot;d&quot;.app/audiowatch"),
            "{p}"
        );
        assert!(!p.contains("a&b"), "raw ampersand leaked into the plist");
    }

    #[test]
    fn xml_escape_covers_the_five_entities() {
        assert_eq!(xml_escape("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(xml_escape("plain/path"), "plain/path");
    }

    #[test]
    fn the_printed_commands_mention_bootstrap_kickstart_and_bootout() {
        let c = commands(Path::new("/x/local.audiowatch.plist"));
        assert!(c.contains("launchctl bootstrap"));
        assert!(c.contains("launchctl kickstart -k"));
        assert!(c.contains("launchctl bootout"));
        assert!(c.contains("/x/local.audiowatch.plist"));
    }
}
