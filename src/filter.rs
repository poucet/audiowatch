//! The allow-list: which processes are expected to make sound and should
//! therefore never interrupt anyone.
//!
//! A match suppresses the *notification* only. The event is still written to
//! the log, carrying the rule that suppressed it, so nothing is ever lost.

use crate::event::Record;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The CFBundleIdentifier the HAL reports for the process.
    Bundle,
    /// The full executable path from `proc_pidpath`.
    Path,
    /// Just the file name at the end of that path — the easy one to type.
    Exe,
}

impl Field {
    pub fn keyword(self) -> &'static str {
        match self {
            Field::Bundle => "allow-bundle",
            Field::Path => "allow-path",
            Field::Exe => "allow-exe",
        }
    }

    pub fn from_keyword(s: &str) -> Option<Self> {
        [Field::Bundle, Field::Path, Field::Exe]
            .into_iter()
            .find(|f| f.keyword() == s)
    }

    fn label(self) -> &'static str {
        match self {
            Field::Bundle => "bundle",
            Field::Path => "path",
            Field::Exe => "exe",
        }
    }

    /// The text this rule tests, pulled out of a record.
    fn value_of(self, record: &Record) -> Option<String> {
        match self {
            Field::Bundle => record.bundle.clone(),
            Field::Path => record.exe.clone(),
            Field::Exe => record
                .exe
                .as_deref()
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub field: Field,
    pub pattern: String,
}

impl Rule {
    pub fn new(field: Field, pattern: impl Into<String>) -> Self {
        Self {
            field,
            pattern: pattern.into(),
        }
    }

    /// How this rule appears in a log line's `allowed:` disposition.
    pub fn label(&self) -> String {
        format!("{} {}", self.field.label(), self.pattern)
    }

    fn matches(&self, record: &Record) -> bool {
        self.field
            .value_of(record)
            .is_some_and(|value| glob_match(&self.pattern, &value))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    pub rules: Vec<Rule>,
}

impl AllowList {
    /// The first rule that matches, or `None` if this event deserves a notification.
    pub fn allows(&self, record: &Record) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matches(record))
    }

    pub fn push(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Case-insensitive glob: `*` matches any run of characters (including `/`),
/// `?` matches exactly one. Everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have matched too little.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;

    fn rec(exe: Option<&str>, bundle: Option<&str>) -> Record {
        let mut r = Record::new(0, Kind::OutputStart, 1);
        r.exe = exe.map(str::to_string);
        r.bundle = bundle.map(str::to_string);
        r
    }

    #[test]
    fn glob_handles_literals_stars_and_question_marks() {
        assert!(glob_match("afplay", "afplay"));
        assert!(!glob_match("afplay", "afplay2"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match(
            "com.google.Chrome*",
            "com.google.Chrome.helper.renderer"
        ));
        assert!(glob_match(
            "/Applications/*.app/*",
            "/Applications/Music.app/Contents/MacOS/Music"
        ));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn glob_backtracks_rather_than_giving_up_on_the_first_star() {
        // The naive greedy matcher fails this one.
        assert!(glob_match(
            "*helper*renderer",
            "com.x.helper.helper.renderer"
        ));
        assert!(glob_match(
            "*.app/*/MacOS/*",
            "/Applications/A.app/Contents/MacOS/A"
        ));
        assert!(!glob_match("*renderer", "renderer.gpu"));
    }

    #[test]
    fn glob_is_case_insensitive_because_nobody_types_bundle_ids_exactly() {
        assert!(glob_match("com.google.chrome*", "com.Google.Chrome.helper"));
        assert!(glob_match("/USR/BIN/AFPLAY", "/usr/bin/afplay"));
    }

    #[test]
    fn a_path_rule_matches_the_full_path_only() {
        let list = AllowList {
            rules: vec![Rule::new(Field::Path, "/Applications/Google Chrome.app/*")],
        };
        assert!(list
            .allows(&rec(
                Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
                None
            ))
            .is_some());
        assert!(list.allows(&rec(Some("/usr/bin/afplay"), None)).is_none());
    }

    #[test]
    fn an_exe_rule_matches_the_basename_so_a_short_name_is_enough() {
        let list = AllowList {
            rules: vec![Rule::new(Field::Exe, "afplay")],
        };
        assert!(list.allows(&rec(Some("/usr/bin/afplay"), None)).is_some());
        assert!(list
            .allows(&rec(Some("/opt/homebrew/bin/afplay"), None))
            .is_some());
        assert!(list.allows(&rec(Some("/usr/bin/afplayer"), None)).is_none());
    }

    #[test]
    fn a_bundle_rule_ignores_a_process_that_has_no_bundle_id() {
        let list = AllowList {
            rules: vec![Rule::new(Field::Bundle, "*")],
        };
        // A bare CLI tool reports no bundle id, so a bundle rule must not
        // silently swallow it -- this is the helper-process case from the brief.
        assert!(list.allows(&rec(Some("/usr/bin/afplay"), None)).is_none());
        assert!(list
            .allows(&rec(Some("/usr/bin/afplay"), Some("com.apple.afplay")))
            .is_some());
    }

    #[test]
    fn the_first_matching_rule_is_the_one_reported() {
        let list = AllowList {
            rules: vec![
                Rule::new(Field::Exe, "afplay"),
                Rule::new(Field::Path, "/usr/bin/*"),
            ],
        };
        let matched = list.allows(&rec(Some("/usr/bin/afplay"), None)).unwrap();
        assert_eq!(matched.label(), "exe afplay");
    }

    #[test]
    fn an_empty_list_allows_nothing() {
        assert!(AllowList::default()
            .allows(&rec(Some("/usr/bin/afplay"), None))
            .is_none());
        assert!(AllowList::default().is_empty());
    }

    #[test]
    fn field_keywords_round_trip() {
        for f in [Field::Bundle, Field::Path, Field::Exe] {
            assert_eq!(Field::from_keyword(f.keyword()), Some(f));
        }
        assert_eq!(Field::from_keyword("allow-everything"), None);
    }
}
