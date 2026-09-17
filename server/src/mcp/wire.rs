//! The bytes the `claude` CLI parses. Every push notification and every tool
//! payload that carries a selection is built here and nowhere else, so the key
//! set the CLI expects has exactly one author.
//!
//! Reconstructed from the shipped VS Code extension and CLI (v2.1.269). Lines
//! and characters are 0-based; the CLI adds one for display.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::selection::{Event, Mention, Selection};

/// A JSON-RPC 2.0 notification: no id, and never answered.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

fn notification(method: &str, params: Value) -> JsonRpcNotification {
    JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: method.to_string(),
        params,
    }
}

pub fn notification_for(event: &Event) -> JsonRpcNotification {
    match event {
        Event::SelectionChanged(selection) => selection_changed(selection),
        Event::AtMentioned(mention) => at_mentioned(mention),
    }
}

/// `selection_changed`: the live selection, pushed editor -> CLI.
pub fn selection_changed(selection: &Selection) -> JsonRpcNotification {
    notification("selection_changed", selection_params(selection))
}

/// The `text / filePath / fileUrl / selection` object shared by the
/// `selection_changed` notification and the selection tools.
pub fn selection_params(selection: &Selection) -> Value {
    let position = |p: lsp_types::Position| json!({"line": p.line, "character": p.character});
    json!({
        "text": selection.text,
        "filePath": selection.path.to_string_lossy(),
        "fileUrl": selection.uri.to_string(),
        "selection": {
            "start": position(selection.range.start),
            "end": position(selection.range.end),
            "isEmpty": selection.is_empty(),
        }
    })
}

/// `at_mentioned`: asks the CLI to insert `@path#Lx-y` into its prompt.
///
/// The line keys are *omitted* for a whole-file mention. The CLI validates them
/// with an optional number, which accepts a missing key and rejects null, so
/// they are inserted only when present rather than serialised from an Option.
pub fn at_mentioned(mention: &Mention) -> JsonRpcNotification {
    let mut params = Map::new();
    params.insert(
        "filePath".to_string(),
        Value::from(mention.path.to_string_lossy().into_owned()),
    );
    if let Some((start, end)) = mention.lines {
        params.insert("lineStart".to_string(), Value::from(start));
        params.insert("lineEnd".to_string(), Value::from(end));
    }
    notification("at_mentioned", Value::Object(params))
}
