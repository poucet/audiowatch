//! How a `record` answer renders as MCP content: a text summary with every
//! file's absolute path (for clients that ignore links), the answer as JSON,
//! and — once the files are finished — one `resource_link` per take, a
//! `file://` URI with `audio/wav`. A link, never an embedded resource: the
//! audio does not travel through the model.

use std::path::Path;

use simply_api::export::rmcp::model::{ContentBlock, Resource};
use simply_api::mcp::MediaContent;

use super::service::Recorded;

/// A `file://` URI for an absolute path, percent-encoding everything but the
/// unreserved characters and `/` (so `BlackHole 16ch` stays a valid URI).
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

fn summary(r: &Recorded) -> String {
    let length = match (r.seconds, r.bars) {
        (_, Some(bars)) => format!("{bars} bar(s) on the MIDI clock"),
        (Some(s), None) => format!("{s} s"),
        (None, None) => "unknown length".into(),
    };
    let mut out = if r.complete {
        format!(
            "recorded {} take(s) of {length}; every file is finished and closed:",
            r.takes.len()
        )
    } else {
        let when = if r.bars.is_some() {
            " They start on the clock's next downbeat (after its next Start, if it has not \
             said where the bar is)."
        } else {
            ""
        };
        format!(
            "recording {} started: {} take(s) of {length}.{when} The files below are NOT \
             readable yet — call await_recording with id {} to get them once they are finished:",
            r.id.unwrap_or_default(),
            r.takes.len(),
            r.id.unwrap_or_default()
        )
    };
    #[cfg(feature = "midi-clock")]
    if let Some(c) = &r.clock {
        out.push_str(&format!(
            "\nclock {:?}: {:.2} of {} bars of {} from bar {}, {:.2} bpm (beats {:.2}–{:.2})",
            c.port,
            c.bars,
            c.bars_asked,
            c.beats_per_bar,
            c.start_bar,
            c.tempo_bpm,
            c.tempo_min_bpm,
            c.tempo_max_bpm
        ));
    }
    for t in &r.takes {
        let length = t
            .duration_s
            .map(|d| format!(", {d:.3} s"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n{} → {} ({} Hz{length})",
            t.spec, t.path, t.sample_rate
        ));
    }
    for w in &r.warnings {
        out.push_str(&format!("\nwarning: {w}"));
    }
    out
}

impl MediaContent for Recorded {
    fn into_content_blocks(self) -> Vec<ContentBlock> {
        let mut blocks = vec![ContentBlock::text(summary(&self))];
        if let Ok(json) = serde_json::to_string_pretty(&self) {
            blocks.push(ContentBlock::text(json));
        }
        if self.complete {
            for t in &self.takes {
                let path = Path::new(&t.path);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| t.path.clone());
                let mut link = Resource::new(file_uri(path), name)
                    .with_title(t.spec.clone())
                    .with_description(format!(
                        "{} {} at {} Hz, {:.3} s",
                        t.device,
                        t.channels,
                        t.sample_rate,
                        t.duration_s.unwrap_or_default()
                    ))
                    .with_mime_type("audio/wav");
                if let Ok(meta) = std::fs::metadata(path) {
                    link = link.with_size(meta.len());
                }
                blocks.push(ContentBlock::resource_link(link));
            }
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rec::service::RecordedTake;
    use audiowatch_record::Channels;

    fn recorded(complete: bool) -> Recorded {
        Recorded {
            id: if complete { None } else { Some(4) },
            complete,
            seconds: Some(2.0),
            bars: None,
            #[cfg(feature = "midi-clock")]
            clock: None,
            takes: vec![RecordedTake {
                spec: "BlackHole 16ch:in:1-2".into(),
                device: "BlackHole 16ch".into(),
                channels: Channels::Pair(1),
                sample_rate: 48_000,
                path: "/Users/x/Music/audio-rec/blackhole 16ch.wav".into(),
                duration_s: complete.then_some(2.0),
                sync: None,
            }],
            warnings: vec![],
        }
    }

    #[test]
    fn a_file_uri_is_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/a b/c#1.wav")),
            "file:///a%20b/c%231.wav"
        );
        assert_eq!(file_uri(Path::new("/r/x-1_2.wav")), "file:///r/x-1_2.wav");
    }

    #[test]
    fn a_finished_take_is_a_resource_link_and_a_path_never_the_audio() {
        let blocks = recorded(true).into_content_blocks();
        let json = serde_json::to_value(&blocks).unwrap();
        let text = json[0]["text"].as_str().unwrap();
        assert!(
            text.contains("/Users/x/Music/audio-rec/blackhole 16ch.wav"),
            "{text}"
        );
        let link = blocks
            .iter()
            .find_map(|b| b.as_resource_link())
            .expect("a resource link");
        assert_eq!(
            link.uri,
            "file:///Users/x/Music/audio-rec/blackhole%2016ch.wav"
        );
        assert_eq!(link.mime_type.as_deref(), Some("audio/wav"));
        assert_eq!(link.name, "blackhole 16ch.wav");
        for b in &json.as_array().unwrap()[..] {
            let kind = b["type"].as_str().unwrap();
            assert!(
                kind == "text" || kind == "resource_link",
                "{kind} must not carry audio"
            );
        }
    }

    #[test]
    fn a_pending_recording_links_nothing_and_says_the_files_are_not_ready() {
        let blocks = recorded(false).into_content_blocks();
        assert!(blocks.iter().all(|b| b.as_resource_link().is_none()));
        let json = serde_json::to_value(&blocks).unwrap();
        assert!(json[0]["text"]
            .as_str()
            .unwrap()
            .contains("NOT readable yet"));
    }

    #[test]
    fn a_pending_bars_recording_says_it_starts_on_the_downbeat() {
        let mut r = recorded(false);
        (r.seconds, r.bars) = (None, Some(4));
        let blocks = r.into_content_blocks();
        let json = serde_json::to_value(&blocks).unwrap();
        let text = json[0]["text"].as_str().unwrap();
        assert!(
            text.contains("4 bar(s) on the MIDI clock") && text.contains("downbeat"),
            "{text}"
        );
    }
}
