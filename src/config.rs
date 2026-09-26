//! The config file: a plain line-based format, because the thing a person does
//! most often with this tool is add one line to the allow-list.
//!
//! `DEFAULT_CONFIG` below is both the documentation and the shipped defaults —
//! it is the exact text written to disk on first run, and it is what
//! `Config::default()` parses. There is no second copy of the default list.

use crate::filter::{AllowList, Field, Rule};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = include_str!("default_config.conf");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub log_path: PathBuf,
    /// Backstop poll interval. `IsRunningOutput` does not send change
    /// notifications, so it has to be sampled; see README.
    pub poll_ms: u64,
    /// Poll interval while a device has just reported that IO started.
    pub fast_poll_ms: u64,
    /// How long to stay in fast mode after such a report.
    pub fast_window_ms: u64,
    pub notify: bool,
    /// Also notify when a process merely connects to the HAL (noisy, but it is
    /// the one thing a short-lived process cannot avoid doing).
    pub notify_connect: bool,
    /// Also notify when a process starts capturing input.
    pub notify_input: bool,
    pub allow: AllowList,
}

impl Default for Config {
    fn default() -> Self {
        // Unwrap is sound: the default config is compiled in and covered by a test.
        parse(DEFAULT_CONFIG).expect("built-in default config must parse")
    }
}

/// `~/.config/audiowatch/config.conf`
pub fn default_config_path() -> PathBuf {
    home().join(".config/audiowatch/config.conf")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand a leading `~/` the way a person writing a config file expects.
pub fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(s),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub line_no: usize,
    pub line: String,
    pub problem: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: {} -- in: {}",
            self.line_no, self.problem, self.line
        )
    }
}

/// Parse a whole config file. Every bad line is reported, not just the first,
/// so one typo does not hide the next.
pub fn parse(text: &str) -> Result<Config, Vec<ConfigError>> {
    let mut cfg = Config {
        log_path: default_log_path(),
        poll_ms: 50,
        fast_poll_ms: 10,
        fast_window_ms: 2_000,
        notify: true,
        notify_connect: false,
        notify_input: false,
        allow: AllowList::default(),
    };
    let mut errors = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (line, ""),
        };
        let mut bad = |problem: &str| {
            errors.push(ConfigError {
                line_no,
                line: raw.trim().to_string(),
                problem: problem.to_string(),
            });
        };

        if let Some(field) = Field::from_keyword(key) {
            if value.is_empty() {
                bad("allow rule needs a pattern");
            } else {
                cfg.allow.push(Rule::new(field, value));
            }
            continue;
        }

        match key {
            "log" => {
                if value.is_empty() {
                    bad("log needs a path");
                } else {
                    cfg.log_path = expand_tilde(value);
                }
            }
            "poll-ms" => match parse_interval(value) {
                Some(n) => cfg.poll_ms = n,
                None => bad("poll-ms needs a number of milliseconds, 1 or more"),
            },
            "fast-poll-ms" => match parse_interval(value) {
                Some(n) => cfg.fast_poll_ms = n,
                None => bad("fast-poll-ms needs a number of milliseconds, 1 or more"),
            },
            "fast-window-ms" => match value.parse::<u64>() {
                Ok(n) => cfg.fast_window_ms = n,
                Err(_) => bad("fast-window-ms needs a number of milliseconds"),
            },
            "notify" => match parse_bool(value) {
                Some(b) => cfg.notify = b,
                None => bad("notify needs on or off"),
            },
            "notify-connect" => match parse_bool(value) {
                Some(b) => cfg.notify_connect = b,
                None => bad("notify-connect needs on or off"),
            },
            "notify-input" => match parse_bool(value) {
                Some(b) => cfg.notify_input = b,
                None => bad("notify-input needs on or off"),
            },
            _ => bad("unknown setting"),
        }
    }

    if errors.is_empty() {
        Ok(cfg)
    } else {
        Err(errors)
    }
}

fn parse_interval(value: &str) -> Option<u64> {
    let n: u64 = value.parse().ok()?;
    (n >= 1).then_some(n)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// `~/Library/Logs/audiowatch/audiowatch.log` — the macOS-idiomatic spot, and
/// visible in Console.app under "Log Reports".
pub fn default_log_path() -> PathBuf {
    home().join("Library/Logs/audiowatch/audiowatch.log")
}

/// Read the config, writing the documented default file first if there is none.
pub fn load_or_create(path: &Path) -> Result<(Config, bool), String> {
    let mut created = false;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        std::fs::write(path, DEFAULT_CONFIG)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        created = true;
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let cfg = parse(&text).map_err(|errs| {
        let mut msg = format!("{} has {} problem(s):\n", path.display(), errs.len());
        for e in errs {
            msg.push_str(&format!("  {e}\n"));
        }
        msg
    })?;
    Ok((cfg, created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Kind, Record};

    fn rec(exe: &str, bundle: Option<&str>) -> Record {
        let mut r = Record::new(0, Kind::OutputStart, 1);
        r.exe = Some(exe.to_string());
        r.bundle = bundle.map(str::to_string);
        r
    }

    #[test]
    fn the_built_in_default_config_parses() {
        let cfg = parse(DEFAULT_CONFIG).expect("default config must parse");
        assert!(cfg.notify, "notifications should be on by default");
        assert!(
            !cfg.allow.is_empty(),
            "default allow-list should not be empty"
        );
    }

    #[test]
    fn the_default_allow_list_covers_the_apps_that_are_meant_to_make_sound() {
        let cfg = Config::default();
        let expected = [
            (
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                Some("com.google.Chrome"),
            ),
            (
                "/Applications/Spotify.app/Contents/MacOS/Spotify",
                Some("com.spotify.client"),
            ),
            (
                "/Applications/Safari.app/Contents/MacOS/Safari",
                Some("com.apple.Safari"),
            ),
            (
                "/System/Applications/Music.app/Contents/MacOS/Music",
                Some("com.apple.Music"),
            ),
            (
                "/Applications/Discord.app/Contents/MacOS/Discord",
                Some("com.hnc.Discord"),
            ),
            ("/usr/bin/osascript", None),
        ];
        for (exe, bundle) in expected {
            assert!(
                cfg.allow.allows(&rec(exe, bundle)).is_some(),
                "default allow-list should cover {exe}"
            );
        }
    }

    #[test]
    fn a_chrome_helper_with_no_bundle_id_is_still_covered_by_path() {
        let cfg = Config::default();
        let helper = "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/154.0.8037.58/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper";
        let matched = cfg
            .allow
            .allows(&rec(helper, None))
            .expect("helper should be allowed");
        assert!(matched.label().starts_with("path "), "{}", matched.label());
    }

    #[test]
    fn the_suspects_are_deliberately_not_allowed_by_default() {
        // These are the plausible sources of an unexplained pop, so the default
        // config must NOT silence them.
        let cfg = Config::default();
        for exe in [
            "/usr/sbin/systemsoundserverd",
            "/System/Library/CoreServices/PowerChime.app/Contents/MacOS/PowerChime",
            "/Library/Audio/Plug-Ins/HAL/ARK.driver/Contents/Resources/Audio Routing Kit (ARK).app/Contents/MacOS/arkaudiod",
            "/opt/homebrew/Cellar/jack/1.9.22_1/bin/jackd",
            "/usr/bin/afplay",
        ] {
            assert!(cfg.allow.allows(&rec(exe, None)).is_none(), "{exe} must not be allowed");
        }
    }

    #[test]
    fn settings_override_the_defaults() {
        let cfg = parse("poll-ms 250\nnotify off\nnotify-connect yes\nlog /tmp/x.log\n").unwrap();
        assert_eq!(cfg.poll_ms, 250);
        assert!(!cfg.notify);
        assert!(cfg.notify_connect);
        assert_eq!(cfg.log_path, PathBuf::from("/tmp/x.log"));
        assert!(
            cfg.allow.is_empty(),
            "an explicit config starts with no allow rules"
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored_including_trailing_comments() {
        let cfg = parse("\n  # a comment\n\npoll-ms 100 # inline comment\n").unwrap();
        assert_eq!(cfg.poll_ms, 100);
    }

    #[test]
    fn allow_patterns_may_contain_spaces_because_paths_do() {
        let cfg = parse("allow-path /Applications/Google Chrome.app/*\n").unwrap();
        assert_eq!(
            cfg.allow.rules[0].pattern,
            "/Applications/Google Chrome.app/*"
        );
    }

    #[test]
    fn every_bad_line_is_reported_with_its_number() {
        let errs = parse("poll-ms nope\nnotify maybe\nallow-bundle\nwhat even\n").unwrap_err();
        assert_eq!(errs.len(), 4);
        assert_eq!(errs[0].line_no, 1);
        assert!(errs[1].problem.contains("on or off"), "{}", errs[1].problem);
        assert!(
            errs[2].problem.contains("needs a pattern"),
            "{}",
            errs[2].problem
        );
        assert!(
            errs[3].problem.contains("unknown setting"),
            "{}",
            errs[3].problem
        );
        assert!(errs[0].to_string().contains("line 1"));
    }

    #[test]
    fn a_zero_poll_interval_is_rejected_rather_than_spinning_a_core() {
        assert!(parse("poll-ms 0\n").is_err());
        assert!(parse("fast-poll-ms 0\n").is_err());
    }

    #[test]
    fn tilde_expands_against_home() {
        let p = expand_tilde("~/Library/Logs/x.log");
        assert!(p.is_absolute());
        assert!(p.starts_with(home()));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn the_default_log_lives_under_library_logs() {
        assert!(default_log_path()
            .to_string_lossy()
            .ends_with("Library/Logs/audiowatch/audiowatch.log"));
    }
}
