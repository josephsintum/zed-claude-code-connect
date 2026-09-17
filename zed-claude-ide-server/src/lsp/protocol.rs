//! The JSON-RPC shapes Zed and this server exchange.
//!
//! Every inbound message is parsed exactly once. `params` stays a borrowed
//! `RawValue` pointing into the frame buffer until a handler asks for its typed
//! form, so a `didOpen` carrying a megabyte of text is not first materialised as
//! a `serde_json::Value` tree and then walked again. That double parse was
//! `tower-lsp`'s shape, and it is most of the 12.6us per cursor move that
//! deleting it recovers.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use lsp_types::{Range, TextDocumentContentChangeEvent, Url};

/// JSON-RPC error codes. The three LSP adds to the standard set are the last two
/// plus `REQUEST_FAILED`, which this server has no use for.
pub mod code {
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// LSP: a request arrived before `initialize`.
    pub const SERVER_NOT_INITIALIZED: i32 = -32002;
}

/// One inbound message, borrowed from the frame buffer it was read into.
#[derive(Debug, Deserialize)]
pub struct Request<'a> {
    pub method: &'a str,
    #[serde(borrow, default)]
    id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub params: Option<&'a RawValue>,
}

impl<'a> Request<'a> {
    /// The request id, or `None` when this is a notification.
    ///
    /// An explicit `"id": null` counts as absent. `Option<T>` deserializes that
    /// to `Some(null)`, not `None`, so asking serde alone would answer a
    /// notification and leave Zed waiting for a reply to a message it never
    /// asked about.
    pub fn id(&self) -> Option<&'a RawValue> {
        self.id.filter(|id| id.get() != "null")
    }

    pub fn is_notification(&self) -> bool {
        self.id().is_none()
    }
}

/// A failed reply. `id` is written back exactly as it arrived, so a string id
/// stays a string and a number stays a number.
///
/// There is no matching type for a successful reply: those are rendered by hand
/// in the loop, because both the id and the result are already JSON text by then
/// and there is nothing left to encode.
#[derive(Debug, Serialize)]
pub struct ErrorResponse<'a> {
    pub jsonrpc: &'static str,
    pub id: &'a RawValue,
    pub error: ResponseError,
}

#[derive(Debug, Serialize)]
pub struct ResponseError {
    pub code: i32,
    pub message: &'static str,
    /// Omitted, never null: the CLI and Zed both validate optional fields as
    /// "missing or of the right type", and reject an explicit null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

impl<'a> ErrorResponse<'a> {
    pub fn new(id: &'a RawValue, code: i32, message: &'static str, data: Option<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            error: ResponseError {
                code,
                message,
                data,
            },
        }
    }
}

/// `textDocument/codeAction`, the request Zed sends on every cursor move.
///
/// Deliberately not `lsp_types::CodeActionParams`: Zed attaches the buffer's
/// diagnostics in `context`, and this server looks only at the range. Unknown
/// fields are skipped rather than built into values nothing reads.
#[derive(Debug, Deserialize)]
pub struct CodeActionParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    pub range: Range,
}

#[derive(Debug, Deserialize)]
pub struct TextDocumentIdentifier {
    pub uri: Url,
}

#[derive(Debug, Deserialize)]
pub struct DidOpenParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentItem,
}

#[derive(Debug, Deserialize)]
pub struct TextDocumentItem {
    pub uri: Url,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct DidChangeParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    #[serde(rename = "contentChanges")]
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Deserialize)]
pub struct DidCloseParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
}
