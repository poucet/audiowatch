//! The record that goes in the log, and its on-disk encoding.
//!
//! The format is one tab-separated line per record, with backslash escapes, so
//! it stays greppable and `awk`-able while round-tripping exactly. The encoder
//! and the decoder below are the only two places that know the column order.

use crate::timefmt;

/// What the HAL told us happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A process connected to the audio HAL. It has not necessarily made a
    /// sound yet — but it cannot make one without doing this first, which is
    /// what makes this the catch-all that a short-lived process cannot dodge.
    Connect,
    /// A process that had connected went away.
    Disconnect,
    /// `kAudioProcessPropertyIsRunningOutput` went 0 -> 1. Real output IO.
    OutputStart,
    /// ... and back to 0.
    OutputStop,
    InputStart,
    InputStop,
    /// Already running output when the watcher started. Recorded, never notified,
    /// so starting the watcher does not announce everything already playing.
    Baseline,
}

impl Kind {
    pub fn wire(self) -> &'static str {
        match self {
            Kind::Connect => "connect",
            Kind::Disconnect => "disconnect",
            Kind::OutputStart => "output-start",
            Kind::OutputStop => "output-stop",
            Kind::InputStart => "input-start",
            Kind::InputStop => "input-stop",
            Kind::Baseline => "baseline",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        const ALL: [Kind; 7] = [
            Kind::Connect,
            Kind::Disconnect,
            Kind::OutputStart,
            Kind::OutputStop,
            Kind::InputStart,
            Kind::InputStop,
            Kind::Baseline,
        ];
        ALL.into_iter().find(|k| k.wire() == s)
    }
}

/// What the watcher did about the event, once the allow-list had its say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// A notification was posted.
    Notified,
    /// Matched the allow-list; logged only. Carries the rule that matched, so
    /// the log says *why* something was suppressed.
    Allowed(String),
    /// Not a notifiable kind, or notifications are off.
    Quiet,
}

impl Disposition {
    fn wire(&self) -> String {
        match self {
            Disposition::Notified => "notified".into(),
            Disposition::Allowed(rule) => format!("allowed:{rule}"),
            Disposition::Quiet => "quiet".into(),
        }
    }

    fn from_wire(s: &str) -> Self {
        match s.strip_prefix("allowed:") {
            Some(rule) => Disposition::Allowed(rule.to_string()),
            None if s == "notified" => Disposition::Notified,
            None => Disposition::Quiet,
        }
    }
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub millis: i64,
    pub kind: Kind,
    pub disposition: Disposition,
    pub pid: i32,
    pub bundle: Option<String>,
    pub exe: Option<String>,
    /// Output devices the process was running on. Joined with `", "` on disk;
    /// no CoreAudio device name on this machine contains that sequence.
    pub devices: Vec<String>,
    /// Anything the watcher wants to remember, e.g. that the process had
    /// already exited by the time its output was noticed.
    pub note: Option<String>,
}

impl Record {
    pub fn new(millis: i64, kind: Kind, pid: i32) -> Self {
        Self {
            millis,
            kind,
            disposition: Disposition::Quiet,
            pid,
            bundle: None,
            exe: None,
            devices: Vec::new(),
            note: None,
        }
    }

    /// The short name to put in a notification: the executable's file name, the
    /// bundle id if there is no path, else the pid.
    pub fn short_name(&self) -> String {
        if let Some(exe) = &self.exe {
            let base = exe.rsplit('/').next().unwrap_or(exe);
            if !base.is_empty() {
                return base.to_string();
            }
        }
        if let Some(b) = &self.bundle {
            return b.clone();
        }
        format!("pid {}", self.pid)
    }

    /// The on-disk line, without its trailing newline.
    pub fn encode(&self) -> String {
        let cols = [
            self.millis.to_string(),
            timefmt::format_local(self.millis),
            self.kind.wire().to_string(),
            self.disposition.wire(),
            self.pid.to_string(),
            self.bundle.clone().unwrap_or_default(),
            self.exe.clone().unwrap_or_default(),
            self.devices.join(", "),
            self.note.clone().unwrap_or_default(),
        ];
        cols.iter()
            .map(|c| escape(c))
            .collect::<Vec<_>>()
            .join("\t")
    }

    pub fn decode(line: &str) -> Option<Self> {
        let cols: Vec<String> = line.split('\t').map(unescape).collect();
        if cols.len() < 9 {
            return None;
        }
        let devices = if cols[7].is_empty() {
            Vec::new()
        } else {
            cols[7].split(", ").map(str::to_string).collect()
        };
        Some(Self {
            millis: cols[0].parse().ok()?,
            kind: Kind::from_wire(&cols[2])?,
            disposition: Disposition::from_wire(&cols[3]),
            pid: cols[4].parse().unwrap_or(-1),
            bundle: some_if_set(&cols[5]),
            exe: some_if_set(&cols[6]),
            devices,
            note: some_if_set(&cols[8]),
        })
    }

    /// The one-line human rendering used by `--tail` and `--since`.
    pub fn human(&self) -> String {
        let mut s = format!(
            "{}  {:<13} {:<28} pid {:<7}",
            timefmt::format_clock(self.millis),
            self.kind.wire(),
            self.short_name(),
            self.pid
        );
        if !self.devices.is_empty() {
            s.push_str(&format!(" dev[{}]", self.devices.join(", ")));
        }
        match &self.disposition {
            Disposition::Allowed(rule) => s.push_str(&format!(" (allowed by {rule})")),
            Disposition::Notified => s.push_str(" (notified)"),
            Disposition::Quiet => {}
        }
        if let Some(exe) = &self.exe {
            s.push_str(&format!("\n{:>14}{}", "", exe));
        }
        if let Some(note) = &self.note {
            s.push_str(&format!("\n{:>14}{}", "", note));
        }
        s
    }
}

fn some_if_set(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            millis: 1_790_000_000_123,
            kind: Kind::OutputStart,
            disposition: Disposition::Notified,
            pid: 51346,
            bundle: Some("com.example.thing".into()),
            exe: Some("/usr/bin/afplay".into()),
            devices: vec!["Scarlett 4i4 USB".into(), "BlackHole 2ch".into()],
            note: None,
        }
    }

    #[test]
    fn every_kind_round_trips_through_its_wire_name() {
        for wire in [
            "connect",
            "disconnect",
            "output-start",
            "output-stop",
            "input-start",
            "input-stop",
            "baseline",
        ] {
            let k = Kind::from_wire(wire).unwrap_or_else(|| panic!("no kind for {wire}"));
            assert_eq!(k.wire(), wire);
        }
        assert_eq!(Kind::from_wire("nonsense"), None);
    }

    #[test]
    fn a_record_round_trips_through_the_log_line() {
        let r = sample();
        let line = r.encode();
        assert!(!line.contains('\n'));
        assert_eq!(Record::decode(&line), Some(r));
    }

    #[test]
    fn the_line_has_exactly_the_nine_columns() {
        assert_eq!(sample().encode().split('\t').count(), 9);
    }

    #[test]
    fn tabs_and_newlines_in_a_path_cannot_break_the_format() {
        let mut r = sample();
        r.exe = Some("/tmp/we\tird\npath\\here".into());
        r.note = Some("two\tcolumns?".into());
        let line = r.encode();
        assert_eq!(
            line.split('\t').count(),
            9,
            "escaping leaked a column: {line}"
        );
        assert_eq!(Record::decode(&line), Some(r));
    }

    #[test]
    fn empty_optional_fields_decode_back_to_none() {
        let r = Record::new(1_790_000_000_000, Kind::Connect, 7);
        let back = Record::decode(&r.encode()).unwrap();
        assert_eq!(back.bundle, None);
        assert_eq!(back.exe, None);
        assert_eq!(back.note, None);
        assert!(back.devices.is_empty());
        assert_eq!(back.disposition, Disposition::Quiet);
    }

    #[test]
    fn a_suppressed_record_remembers_the_rule_that_matched() {
        let mut r = sample();
        r.disposition = Disposition::Allowed("path /Applications/Google Chrome.app/*".into());
        let back = Record::decode(&r.encode()).unwrap();
        assert_eq!(
            back.disposition,
            Disposition::Allowed("path /Applications/Google Chrome.app/*".into())
        );
        assert!(back.human().contains("allowed by path /Applications"));
    }

    #[test]
    fn garbage_lines_are_rejected_rather_than_guessed_at() {
        assert_eq!(Record::decode(""), None);
        assert_eq!(Record::decode("# a comment"), None);
        assert_eq!(Record::decode("1\t2\t3"), None);
        // Nine columns but an unknown kind is still a reject.
        assert_eq!(Record::decode("1\tx\tbogus\tquiet\t1\t\t\t\t"), None);
        // Nine columns but an unparseable timestamp is a reject.
        assert_eq!(Record::decode("nope\tx\tconnect\tquiet\t1\t\t\t\t"), None);
    }

    #[test]
    fn short_name_prefers_the_executable_then_bundle_then_pid() {
        let mut r = sample();
        assert_eq!(r.short_name(), "afplay");
        r.exe = None;
        assert_eq!(r.short_name(), "com.example.thing");
        r.bundle = None;
        assert_eq!(r.short_name(), "pid 51346");
    }

    #[test]
    fn human_names_the_device_so_the_line_answers_which_output() {
        let h = sample().human();
        assert!(h.contains("Scarlett 4i4 USB"), "{h}");
        assert!(h.contains("/usr/bin/afplay"), "{h}");
    }
}
