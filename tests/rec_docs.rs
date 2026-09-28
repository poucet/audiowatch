//! The tool registry is well formed: every tool and parameter is described,
//! so what an agent reads is never an empty string.

use audiowatch::rec::AUDIO_REC_API_META;

#[test]
fn every_tool_is_documented() {
    let issues = AUDIO_REC_API_META.lint();
    assert!(issues.is_empty(), "{issues:#?}");
    assert_eq!(AUDIO_REC_API_META.tool_count(), 3);
}
