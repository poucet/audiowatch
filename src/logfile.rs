//! The append-only log. This is the tool's primary output: a notification can
//! be missed, dismissed or never shown, but the line is on disk before the
//! notification is even attempted.

use crate::event::{Kind, Record};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const HEADER: &str = "# audiowatch log v1 -- columns, tab-separated and backslash-escaped: epoch_ms, local_time, kind, disposition, pid, bundle, exe, devices, note";

/// Rotate once the log passes this size, so a login daemon cannot fill a disk.
pub const ROTATE_BYTES: u64 = 8 * 1024 * 1024;

pub struct Writer {
    file: File,
    path: PathBuf,
}

impl Writer {
    /// Open for appending, creating the directory, the file and its header as
    /// needed, and rotating the previous log if it has grown too large.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.metadata().map(|m| m.len()).unwrap_or(0) >= ROTATE_BYTES {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        let fresh = !path.exists() || path.metadata().map(|m| m.len()).unwrap_or(0) == 0;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if fresh {
            writeln!(file, "{HEADER}")?;
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and get it onto the disk before returning. Events are
    /// infrequent, so the `sync_data` is free in practice and means a hard
    /// reboot cannot lose the line that explains the noise.
    pub fn append(&mut self, record: &Record) -> std::io::Result<()> {
        writeln!(self.file, "{}", record.encode())?;
        self.file.flush()?;
        self.file.sync_data()
    }
}

/// Everything readable in the log, plus a count of lines that were not records.
#[derive(Debug, Default)]
pub struct Log {
    pub records: Vec<Record>,
    pub unreadable: usize,
}

impl Log {
    pub fn parse(text: &str) -> Self {
        let mut log = Log::default();
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match Record::decode(line) {
                Some(r) => log.records.push(r),
                None => log.unreadable += 1,
            }
        }
        log
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Records matching a query, oldest first.
    pub fn select(&self, query: &Query) -> Vec<&Record> {
        let mut hits: Vec<&Record> = self
            .records
            .iter()
            .filter(|r| query.since.is_none_or(|s| r.millis >= s))
            .filter(|r| query.kinds.as_ref().is_none_or(|ks| ks.contains(&r.kind)))
            .collect();
        if let Some(n) = query.last {
            let skip = hits.len().saturating_sub(n);
            hits = hits.split_off(skip);
        }
        hits
    }
}

#[derive(Debug, Default, Clone)]
pub struct Query {
    /// Epoch milliseconds; records at or after this are included.
    pub since: Option<i64>,
    /// Keep only the last N matches.
    pub last: Option<usize>,
    /// Keep only these kinds.
    pub kinds: Option<Vec<Kind>>,
}

impl Query {
    /// The kinds worth showing by default: sound actually started or stopped.
    pub fn sound_kinds() -> Vec<Kind> {
        vec![
            Kind::OutputStart,
            Kind::OutputStop,
            Kind::InputStart,
            Kind::InputStop,
            Kind::Baseline,
        ]
    }
}

/// Stream records appended to the log from now on, calling `on_record` for each.
/// Used by `--tail --follow`; returns only on an IO error.
pub fn follow(
    path: &Path,
    poll: std::time::Duration,
    mut on_record: impl FnMut(&Record),
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    loop {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(pos);
        if len < pos {
            // Rotated out from under us; start again at the new beginning.
            file = File::open(path)?;
            pos = 0;
        }
        if len > pos {
            file.seek(SeekFrom::Start(pos))?;
            let mut text = String::new();
            let read = BufReader::new(&mut file).read_to_string(&mut text)?;
            // Only consume whole lines; a partial last line is left for next time.
            let consumed = match text.rfind('\n') {
                Some(i) => i + 1,
                None => 0,
            };
            for line in text[..consumed].lines() {
                if let Some(r) = Record::decode(line) {
                    on_record(&r);
                }
            }
            pos += consumed as u64;
            let _ = read;
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Disposition;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "audiowatch-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn join(&self, s: &str) -> PathBuf {
            self.0.join(s)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(millis: i64, kind: Kind, exe: &str) -> Record {
        let mut r = Record::new(millis, kind, 42);
        r.exe = Some(exe.to_string());
        r
    }

    #[test]
    fn a_new_log_gets_a_header_and_then_appends() {
        let d = TempDir::new("header");
        let p = d.join("deep/nested/a.log");
        let mut w = Writer::open(&p).unwrap();
        w.append(&rec(1000, Kind::OutputStart, "/usr/bin/afplay"))
            .unwrap();
        w.append(&rec(2000, Kind::OutputStop, "/usr/bin/afplay"))
            .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# audiowatch log v1"));
        assert_eq!(text.lines().count(), 3);
        assert_eq!(text.lines().next().unwrap(), HEADER);
    }

    #[test]
    fn reopening_appends_rather_than_truncating_and_writes_one_header_only() {
        let d = TempDir::new("append");
        let p = d.join("a.log");
        Writer::open(&p)
            .unwrap()
            .append(&rec(1000, Kind::Connect, "/a"))
            .unwrap();
        Writer::open(&p)
            .unwrap()
            .append(&rec(2000, Kind::Connect, "/b"))
            .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("# audiowatch log v1").count(), 1);
        let log = Log::parse(&text);
        assert_eq!(log.records.len(), 2);
        assert_eq!(log.records[0].exe.as_deref(), Some("/a"));
        assert_eq!(log.records[1].exe.as_deref(), Some("/b"));
    }

    #[test]
    fn what_is_written_is_what_is_read_back() {
        let d = TempDir::new("roundtrip");
        let p = d.join("a.log");
        let mut original = rec(1_790_000_000_123, Kind::OutputStart, "/usr/bin/afplay");
        original.bundle = Some("com.example".into());
        original.devices = vec!["Scarlett 4i4 USB".into()];
        original.disposition = Disposition::Allowed("exe afplay".into());
        original.note = Some("process had already exited".into());
        Writer::open(&p).unwrap().append(&original).unwrap();
        let log = Log::load(&p).unwrap();
        assert_eq!(log.records, vec![original]);
        assert_eq!(log.unreadable, 0);
    }

    #[test]
    fn a_missing_log_reads_as_empty_rather_than_failing() {
        let log = Log::load(Path::new("/nonexistent/audiowatch/nope.log")).unwrap();
        assert!(log.records.is_empty());
    }

    #[test]
    fn corrupt_lines_are_counted_not_fatal() {
        let log = Log::parse(&format!(
            "{HEADER}\nthis is not a record\n{}\n\nalso not\n",
            rec(1000, Kind::Connect, "/a").encode()
        ));
        assert_eq!(log.records.len(), 1);
        assert_eq!(log.unreadable, 2);
    }

    #[test]
    fn since_keeps_only_records_at_or_after_the_cutoff() {
        let log = Log {
            records: vec![
                rec(1000, Kind::OutputStart, "/a"),
                rec(2000, Kind::OutputStart, "/b"),
                rec(3000, Kind::OutputStart, "/c"),
            ],
            unreadable: 0,
        };
        let q = Query {
            since: Some(2000),
            ..Default::default()
        };
        let hits = log.select(&q);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].exe.as_deref(), Some("/b"));
    }

    #[test]
    fn last_keeps_the_newest_n_in_order() {
        let log = Log {
            records: (1..=5)
                .map(|i| rec(i * 1000, Kind::OutputStart, "/x"))
                .collect(),
            unreadable: 0,
        };
        let hits = log.select(&Query {
            last: Some(2),
            ..Default::default()
        });
        assert_eq!(
            hits.iter().map(|r| r.millis).collect::<Vec<_>>(),
            vec![4000, 5000]
        );
        // Asking for more than exists is not an error.
        assert_eq!(
            log.select(&Query {
                last: Some(99),
                ..Default::default()
            })
            .len(),
            5
        );
    }

    #[test]
    fn a_kind_filter_hides_the_connect_chatter() {
        let log = Log {
            records: vec![
                rec(1000, Kind::Connect, "/a"),
                rec(2000, Kind::OutputStart, "/b"),
                rec(3000, Kind::Disconnect, "/a"),
            ],
            unreadable: 0,
        };
        let q = Query {
            kinds: Some(Query::sound_kinds()),
            ..Default::default()
        };
        let hits = log.select(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, Kind::OutputStart);
    }

    #[test]
    fn since_and_last_compose() {
        let log = Log {
            records: (1..=10)
                .map(|i| rec(i * 1000, Kind::OutputStart, "/x"))
                .collect(),
            unreadable: 0,
        };
        let hits = log.select(&Query {
            since: Some(5000),
            last: Some(2),
            ..Default::default()
        });
        assert_eq!(
            hits.iter().map(|r| r.millis).collect::<Vec<_>>(),
            vec![9000, 10000]
        );
    }

    #[test]
    fn an_oversized_log_is_rotated_aside_on_open() {
        let d = TempDir::new("rotate");
        let p = d.join("a.log");
        std::fs::write(&p, vec![b'x'; ROTATE_BYTES as usize + 1]).unwrap();
        Writer::open(&p)
            .unwrap()
            .append(&rec(1000, Kind::Connect, "/a"))
            .unwrap();
        assert!(
            p.with_extension("log.1").exists(),
            "previous log should be kept aside"
        );
        let log = Log::load(&p).unwrap();
        assert_eq!(log.records.len(), 1);
    }
}
