//! The editor side of the bridge, end to end: a fake Zed speaks LSP over an
//! in-memory pipe, and a fake CLI on the WebSocket asserts what comes out.
//!
//! This is the path the whole feature rides on -- Zed issues codeAction with the
//! live selection range, the companion turns it into selection_changed -- and
//! it was previously covered by nothing but a live watch.

mod common;

use std::time::Duration;

use common::{boot, range};
use lsp_types::TextDocumentContentChangeEvent;
use serde_json::json;

/// Wide enough for the rig's 50ms debounce plus scheduling noise.
const ARRIVAL: Duration = Duration::from_secs(3);
/// Long enough that a debounced frame would have arrived if one were coming.
const QUIET: Duration = Duration::from_millis(400);

#[tokio::test]
async fn initialize_advertises_only_sync_and_code_action() {
    let rig = boot().await;
    let caps = &rig.capabilities;
    let mut keys: Vec<&str> = caps
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["codeActionProvider", "textDocumentSync"],
        "this server attaches to every buffer; anything else advertised is a request \
         Zed routes here instead of to the real language server: {caps}"
    );
    assert_eq!(caps["textDocumentSync"], 2, "incremental");
    assert_eq!(caps["codeActionProvider"], true);
}

#[tokio::test]
async fn code_action_returns_an_empty_list() {
    let mut rig = boot().await;
    let uri = rig.file("a.txt", "hello\n");
    rig.zed.open(&uri, "hello\n").await;
    let actions = rig.zed.select(&uri, range(0, 0, 0, 5)).await;
    assert_eq!(
        actions,
        json!([]),
        "an advertised action lights up every line in Zed"
    );
}

#[tokio::test]
async fn a_selection_in_an_open_buffer_reaches_the_cli() {
    let mut rig = boot().await;
    let uri = rig.file("a.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    rig.zed
        .open(&uri, "fn main() {\n    println!(\"hi\");\n}\n")
        .await;

    rig.zed.select(&uri, range(1, 4, 1, 19)).await;

    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .expect("selection_changed");
    assert_eq!(sel["text"], "println!(\"hi\");");
    assert_eq!(
        sel["filePath"],
        uri.to_file_path().unwrap().to_str().unwrap()
    );
    assert_eq!(sel["selection"]["start"]["line"], 1);
    assert_eq!(sel["selection"]["end"]["character"], 19);
    assert_eq!(sel["selection"]["isEmpty"], false);
}

#[tokio::test]
async fn unsaved_edits_show_in_the_selection_text() {
    let mut rig = boot().await;
    let uri = rig.file("a.rs", "let x = 1;\n");
    rig.zed.open(&uri, "let x = 1;\n").await;
    rig.zed
        .change(
            &uri,
            2,
            vec![TextDocumentContentChangeEvent {
                range: Some(range(0, 8, 0, 9)),
                range_length: None,
                text: "42".to_string(),
            }],
        )
        .await;

    rig.zed.select(&uri, range(0, 0, 0, 11)).await;

    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .unwrap();
    assert_eq!(
        sel["text"], "let x = 42;",
        "the mirror, not the file on disk"
    );
}

#[tokio::test]
async fn a_burst_of_cursor_moves_yields_one_notification_for_the_last() {
    let mut rig = boot().await;
    let text = "alpha\nbravo\ncharlie\ndelta\necho\n";
    let uri = rig.file("a.txt", text);
    rig.zed.open(&uri, text).await;

    // Holding shift-down through the buffer.
    for end in 1..=4 {
        rig.zed.select(&uri, range(0, 0, end, 0)).await;
    }

    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .unwrap();
    assert_eq!(
        sel["selection"]["end"]["line"], 4,
        "only the settled selection"
    );
    rig.cli.expect_quiet(QUIET).await;
}

#[tokio::test]
async fn an_identical_settled_selection_is_not_resent() {
    let mut rig = boot().await;
    let uri = rig.file("a.txt", "same\n");
    rig.zed.open(&uri, "same\n").await;

    rig.zed.select(&uri, range(0, 0, 0, 4)).await;
    rig.cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .unwrap();

    rig.zed.select(&uri, range(0, 0, 0, 4)).await;
    rig.cli.expect_quiet(QUIET).await;
}

/// A bare cursor reports no text, and does not read the file to find that out.
///
/// The offsets of a zero-width range are equal whatever the document says, so the
/// answer is known without looking. This pins the contract rather than the shortcut:
/// the frame the CLI receives must be identical either way, which is what makes the
/// optimisation in `code_action` safe to keep.
#[tokio::test]
async fn a_bare_cursor_in_an_unopened_document_reports_no_text() {
    let mut rig = boot().await;
    let uri = rig.file("unopened.txt", "from the disk\n");

    rig.zed.select(&uri, range(0, 5, 0, 5)).await;

    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .expect("a cursor move still reaches the CLI");
    assert_eq!(sel["text"], "", "a zero-width range has no text to report");
    assert_eq!(sel["selection"]["isEmpty"], true);
}

#[tokio::test]
async fn a_never_opened_document_falls_back_to_disk() {
    let mut rig = boot().await;
    let uri = rig.file("unopened.txt", "from the disk\n");

    rig.zed.select(&uri, range(0, 5, 0, 8)).await;

    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .unwrap();
    assert_eq!(sel["text"], "the");
}

/// The disk fallback and the buffer mirror must agree on what a range means.
/// The old fallback returned "" for a single-line range past end-of-line and
/// joined lines with "\n" whatever the file used; the mirror clamps and keeps
/// the bytes as they are.
#[tokio::test]
async fn the_disk_fallback_clamps_and_preserves_line_endings_like_the_mirror() {
    let mut rig = boot().await;
    let uri = rig.file("crlf.txt", "one\r\ntwo\r\n");

    rig.zed.select(&uri, range(0, 0, 0, 99)).await;
    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .unwrap();
    assert_eq!(
        sel["text"], "one",
        "past end-of-line clamps rather than yielding nothing"
    );

    rig.zed.select(&uri, range(0, 0, 1, 3)).await;
    let sel = rig
        .cli
        .next_notification("selection_changed", ARRIVAL)
        .await
        .unwrap();
    assert_eq!(
        sel["text"], "one\r\ntwo",
        "CRLF is the file's, not ours to rewrite"
    );
}

#[tokio::test]
async fn an_oversized_document_falls_back_to_disk() {
    let mut rig = boot().await;
    let big = "x".repeat(4 * 1024 * 1024 + 1);
    let uri = rig.file("big.txt", &big);
    rig.zed.open(&uri, &big).await;

    rig.zed.select(&uri, range(0, 0, 0, 5)).await;

    let sel = rig
        .cli
        .next_notification("selection_changed", Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(sel["text"], "xxxxx");
}

#[tokio::test]
async fn closing_the_editor_side_ends_the_lsp_server() {
    let rig = boot().await;
    rig.zed.close().await;
}
