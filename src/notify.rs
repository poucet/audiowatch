//! macOS notifications, posted through `osascript`.
//!
//! Why this route: a notification posted by `display notification` is delivered
//! by Notification Center on behalf of Script Editor, which needs no app bundle
//! of our own and works from a LaunchAgent. It was verified on this machine by
//! posting one and finding `usernoted` log it as "scheduled for delivery" ->
//! "Delivering" -> "Presenting"; `audiowatch --test-notify` repeats that check.
//!
//! The notification never carries a sound. This tool does not make audio.

use crate::event::{Kind, Record};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub subtitle: String,
    pub body: String,
}

/// What to say about a record. One place, so the notification and the
/// `--test-notify` preview cannot drift apart.
pub fn message_for(record: &Record) -> Message {
    let what = match record.kind {
        Kind::OutputStart => "Audio output",
        Kind::InputStart => "Audio input",
        Kind::Connect => "Audio connect",
        other => other.wire(),
    };
    let subtitle = if record.devices.is_empty() {
        format!("pid {}", record.pid)
    } else {
        record.devices.join(", ")
    };
    let mut body = record.exe.clone().unwrap_or_else(|| record.short_name());
    body.push_str(&format!(" (pid {})", record.pid));
    if let Some(note) = &record.note {
        body.push_str(&format!(" -- {note}"));
    }
    Message {
        title: format!("{what}: {}", record.short_name()),
        subtitle,
        body,
    }
}

/// Escape a Rust string into an AppleScript string literal's contents.
/// AppleScript literals take no `\n`, so newlines and other control characters
/// become spaces rather than breaking the script.
pub fn applescript_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The exact AppleScript that gets run. Deliberately has no `sound name`.
pub fn script_for(message: &Message) -> String {
    format!(
        "display notification \"{}\" with title \"{}\" subtitle \"{}\"",
        applescript_escape(&message.body),
        applescript_escape(&message.title),
        applescript_escape(&message.subtitle),
    )
}

/// Post the notification. Returns the error text if `osascript` refused.
pub fn post(message: &Message) -> Result<(), String> {
    let script = script_for(message);
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| format!("cannot run /usr/bin/osascript: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "osascript exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Ask the unified log whether Notification Center actually presented something
/// on Script Editor's behalf in the last `secs` seconds. This is how the tool
/// proves a notification appeared instead of assuming it.
pub fn recently_delivered(secs: u32) -> Result<bool, String> {
    let out = Command::new("/usr/bin/log")
        .args([
            "show",
            "--last",
            &format!("{secs}s"),
            "--style",
            "compact",
            "--predicate",
            "process == \"usernoted\"",
        ])
        .output()
        .map_err(|e| format!("cannot run /usr/bin/log: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().any(|l| {
        l.contains("com.apple.ScriptEditor2")
            && (l.contains("Presenting") || l.contains("scheduled for delivery"))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: Kind, exe: &str, devices: &[&str]) -> Record {
        let mut r = Record::new(0, kind, 51346);
        r.exe = Some(exe.to_string());
        r.devices = devices.iter().map(|s| s.to_string()).collect();
        r
    }

    #[test]
    fn a_message_names_the_executable_and_the_device() {
        let m = message_for(&rec(
            Kind::OutputStart,
            "/usr/bin/afplay",
            &["Scarlett 4i4 USB"],
        ));
        assert_eq!(m.title, "Audio output: afplay");
        assert_eq!(m.subtitle, "Scarlett 4i4 USB");
        assert!(m.body.contains("/usr/bin/afplay"), "{}", m.body);
        assert!(m.body.contains("pid 51346"), "{}", m.body);
    }

    #[test]
    fn with_no_device_known_the_subtitle_still_says_something_useful() {
        let m = message_for(&rec(Kind::OutputStart, "/usr/bin/afplay", &[]));
        assert_eq!(m.subtitle, "pid 51346");
    }

    #[test]
    fn a_note_such_as_already_exited_reaches_the_notification() {
        let mut r = rec(Kind::OutputStart, "/usr/bin/afplay", &["X"]);
        r.note = Some("process had already exited".into());
        assert!(message_for(&r).body.contains("already exited"));
    }

    #[test]
    fn input_and_connect_get_their_own_wording() {
        assert!(message_for(&rec(Kind::InputStart, "/a/b", &[]))
            .title
            .starts_with("Audio input:"));
        assert!(message_for(&rec(Kind::Connect, "/a/b", &[]))
            .title
            .starts_with("Audio connect:"));
    }

    #[test]
    fn quotes_and_backslashes_in_a_path_cannot_break_out_of_the_script() {
        let nasty = "/tmp/a\"b\\c.app";
        let escaped = applescript_escape(nasty);
        assert_eq!(escaped, "/tmp/a\\\"b\\\\c.app");
        let script = script_for(&Message {
            title: nasty.into(),
            subtitle: String::new(),
            body: String::new(),
        });
        // Exactly six unescaped quotes: the three pairs of string delimiters.
        let bare = script
            .char_indices()
            .filter(|(i, c)| *c == '"' && (*i == 0 || script.as_bytes()[i - 1] != b'\\'))
            .count();
        assert_eq!(bare, 6, "{script}");
    }

    #[test]
    fn control_characters_become_spaces_so_the_script_stays_one_line() {
        let escaped = applescript_escape("a\nb\tc\rd");
        assert_eq!(escaped, "a b c d");
        assert!(!script_for(&message_for(&{
            let mut r = rec(Kind::OutputStart, "/a\nb", &[]);
            r.note = Some("x\ny".into());
            r
        }))
        .contains('\n'));
    }

    #[test]
    fn the_script_never_asks_for_a_sound() {
        let script = script_for(&message_for(&rec(
            Kind::OutputStart,
            "/usr/bin/afplay",
            &["X"],
        )));
        assert!(!script.contains("sound"), "{script}");
        assert!(script.starts_with("display notification "), "{script}");
    }
}
