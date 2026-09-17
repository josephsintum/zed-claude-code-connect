//! The editor side: what Zed sends over LSP, and what is done with it.
//!
//! This is not a language server. It attaches to every buffer only to observe
//! the active file and selection, so it advertises document sync (for the buffer
//! mirror) and code actions (the one request Zed sends with the live selection
//! range), and nothing else.
//!
//! One blocking loop on one thread does all of it: read a frame, parse it once,
//! dispatch, write, flush. That is also what makes two `didChange` notifications
//! for one buffer apply in the order they arrived, and why neither the document
//! store nor the writer needs a lock. Do not introduce concurrent dispatch here
//! without revisiting both.

use std::borrow::Cow;
use std::io::{BufRead, Write};
use std::panic::AssertUnwindSafe;

use lsp_types::{
    CodeActionProviderCapability, InitializeParams, InitializeResult, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use serde_json::value::RawValue;
use tracing::{debug, info, warn};

use super::documents::DocumentStore;
use super::framing::{FrameReader, FrameWriter};
use super::protocol::{code, CodeActionParams, DidChangeParams, DidCloseParams, DidOpenParams};
use super::protocol::{ErrorResponse, Request};
use crate::selection::{Selection, SelectionTracker};

/// What a handler answers with. `Result` carries the `result` member already
/// rendered as JSON text, so the two constants on the hot path -- `[]` for a code
/// action and `null` for everything with no answer -- cost no serialization at
/// all.
enum Reply<'a> {
    /// Nothing goes back: a notification, or a request answered by silence.
    Silent,
    Result(Cow<'a, str>),
    Error {
        code: i32,
        message: &'static str,
        data: Option<String>,
    },
}

impl Reply<'_> {
    const EMPTY_ACTIONS: Reply<'static> = Reply::Result(Cow::Borrowed("[]"));
    const NULL: Reply<'static> = Reply::Result(Cow::Borrowed("null"));
}

/// Serve LSP over a blocking byte stream pair until the input ends or the client
/// sends `exit`.
///
/// The binary passes stdio. The tests pass an OS pipe and play Zed's part, which
/// is the only way to exercise didOpen -> codeAction -> selection_changed
/// without an editor.
///
/// `exit` matters because it is how Zed actually ends a language server, on quit
/// and on restart alike: `shutdown`, then `exit`, then it kills the process. It
/// does not close stdin first. A server that kept reading until EOF would sit
/// until the kill arrived and its lock file would outlive the process. Observed
/// live: every restart left a dead "Zed" entry in the CLI's /ide picker.
pub fn serve_lsp<R: BufRead, W: Write>(input: R, output: W, tracker: SelectionTracker) {
    let mut server = LspServer::new(tracker);
    let mut reader = FrameReader::new(input);
    let mut writer = FrameWriter::new(output);
    // Both buffers are reused for the life of the session, so a steady stream of
    // cursor moves settles on no allocation for the frame or the response.
    let mut body = Vec::new();
    let mut frame = String::new();

    loop {
        match reader.next_frame(&mut body) {
            Ok(true) => {}
            Ok(false) => {
                info!("LSP input ended");
                return;
            }
            Err(e) => {
                warn!("LSP input failed: {e}");
                return;
            }
        }

        let request: Request = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                // No usable id, so there is nobody to answer; a reply carrying a
                // null id is one the client cannot match to anything either.
                warn!("undecodable LSP frame dropped: {e}");
                continue;
            }
        };

        if request.method == "exit" {
            info!("LSP client sent exit");
            return;
        }

        let id = request.id();
        let reply = server.dispatch(&request);

        let Some(id) = id else {
            // A notification is never answered, whatever the handler decided.
            continue;
        };
        match reply {
            Reply::Silent => {}
            Reply::Result(result) => {
                frame.clear();
                // Hand-rendered rather than serialized: `id` is already JSON text
                // and `result` is too, so there is nothing left to encode.
                frame.push_str(r#"{"jsonrpc":"2.0","id":"#);
                frame.push_str(id.get());
                frame.push_str(r#","result":"#);
                frame.push_str(&result);
                frame.push('}');
                if let Err(e) = writer.write_frame(frame.as_bytes()) {
                    warn!("LSP output failed: {e}");
                    return;
                }
            }
            Reply::Error {
                code,
                message,
                data,
            } => {
                let response = ErrorResponse::new(id, code, message, data);
                let encoded = serde_json::to_vec(&response).expect("a response always encodes");
                if let Err(e) = writer.write_frame(&encoded) {
                    warn!("LSP output failed: {e}");
                    return;
                }
            }
        }
    }
}

struct LspServer {
    /// Where every selection Zed reports goes; debouncing happens there.
    tracker: SelectionTracker,
    /// Mirror of the buffers Zed has open, so selection text reflects unsaved edits.
    documents: DocumentStore,
    initialized: bool,
    shut_down: bool,
}

impl LspServer {
    fn new(tracker: SelectionTracker) -> Self {
        Self {
            tracker,
            documents: DocumentStore::new(),
            initialized: false,
            shut_down: false,
        }
    }

    fn dispatch<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        // A panic in a handler must cost one request, not the session. Without
        // this Zed waits forever for a response that is never coming, and the
        // lock file outlives a process nobody is talking to.
        match std::panic::catch_unwind(AssertUnwindSafe(|| self.route(request))) {
            Ok(reply) => reply,
            Err(_) => {
                warn!("handler for {} panicked", request.method);
                Reply::Error {
                    code: code::INTERNAL_ERROR,
                    message: "Internal error",
                    data: None,
                }
            }
        }
    }

    fn route<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        let notification = request.is_notification();

        if request.method == "initialize" {
            return if self.initialized {
                Reply::Error {
                    code: code::INVALID_REQUEST,
                    message: "Invalid request",
                    data: None,
                }
            } else {
                self.initialized = true;
                self.initialize(request)
            };
        }

        // Ordering before the handshake, per LSP: a request is refused, a
        // notification is dropped without a word. `exit` never reaches here.
        if !self.initialized {
            return if notification {
                debug!("dropping {} received before initialize", request.method);
                Reply::Silent
            } else {
                Reply::Error {
                    code: code::SERVER_NOT_INITIALIZED,
                    message: "Server not initialized",
                    data: None,
                }
            };
        }
        // After `shutdown` the only thing left to honour is `exit`.
        if self.shut_down && !notification {
            return Reply::Error {
                code: code::INVALID_REQUEST,
                message: "Invalid request",
                data: None,
            };
        }

        match request.method {
            "initialized" => {
                info!("LSP server initialized");
                Reply::Silent
            }
            "shutdown" => {
                info!("LSP server shutting down");
                self.shut_down = true;
                Reply::NULL
            }
            "textDocument/didOpen" => self.did_open(request),
            "textDocument/didChange" => self.did_change(request),
            "textDocument/didClose" => self.did_close(request),
            "textDocument/didSave" => Reply::Silent,
            "textDocument/codeAction" => self.code_action(request),
            // Zed asks for syntax-aware selection expansion here. This server has
            // no parser, and an earlier stub answered with a synthetic
            // one-character range at each position -- which it also pushed out as
            // `selection_changed`, so any cursor movement overwrote the real
            // selection captured by `code_action`. Answering null lets Zed fall
            // back to its own tree-sitter expansion.
            "textDocument/selectionRange" => Reply::NULL,
            other if notification => {
                debug!("ignoring notification {other}");
                Reply::Silent
            }
            other => Reply::Error {
                code: code::METHOD_NOT_FOUND,
                message: "Method not found",
                data: Some(other.to_string()),
            },
        }
    }

    fn initialize<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        info!("LSP server initializing");
        // Everything Zed told us about itself, for when a capability question
        // comes up. Too much for the normal log, and the only reason this cold
        // path decodes the full `InitializeParams` at all.
        if tracing::enabled!(tracing::Level::DEBUG) {
            if let Some(params) = decode::<InitializeParams>(request) {
                debug!("client capabilities: {:?}", params.capabilities);
                for folder in params.workspace_folders.iter().flatten() {
                    debug!("workspace folder: {}", folder.uri);
                }
            }
        }

        let result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                // This server is not a language server. It attaches to every buffer
                // only to observe the active file and selection, so it must stay
                // invisible: anything advertised here is a request Zed will route to
                // it instead of, or alongside, the project's real language server.
                //
                // Previously it claimed definition/references/documentSymbol/
                // workspaceSymbol with no handler behind any of them, so Zed's
                // edit-prediction filled the log with "Method not found" on every
                // cursor move; and it answered every completion request with three
                // "@claude ..." items, which appeared in every popup in every file.
                //
                // Load-bearing: Zed passes the live selection range on code
                // actions, which is what drives selection_changed.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "Claude Code Language Server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        };
        Reply::Result(Cow::Owned(
            serde_json::to_string(&result).expect("the capabilities always encode"),
        ))
    }

    fn did_open<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        let Some(params) = decode::<DidOpenParams>(request) else {
            return Reply::Silent;
        };
        debug!("Document opened: {}", params.text_document.uri);
        self.documents
            .open(params.text_document.uri, params.text_document.text);
        Reply::Silent
    }

    fn did_change<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        let Some(params) = decode::<DidChangeParams>(request) else {
            return Reply::Silent;
        };
        debug!("Document changed: {}", params.text_document.uri);
        self.documents
            .apply(&params.text_document.uri, params.content_changes);
        Reply::Silent
    }

    fn did_close<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        let Some(params) = decode::<DidCloseParams>(request) else {
            return Reply::Silent;
        };
        debug!("Document closed: {}", params.text_document.uri);
        self.documents.close(&params.text_document.uri);
        Reply::Silent
    }

    fn code_action<'a>(&mut self, request: &Request<'a>) -> Reply<'a> {
        let Some(params) = decode::<CodeActionParams>(request) else {
            return Reply::Error {
                code: code::INVALID_REQUEST,
                message: "Invalid request",
                data: None,
            };
        };

        // `Url::path()` is percent-encoded, so a project path containing a space
        // arrives as `/Users/me/My%20Project/a.rs` -- unopenable, and meaningless to
        // the CLI. `to_file_path()` gives the real path.
        let path = uri_to_path(&params.text_document.uri);
        // The mirror reflects unsaved edits; disk is the fallback for documents
        // too large to mirror, or never opened through document sync. Both go
        // through the same range code, so a position means the same thing
        // whichever source answered, and the bare-cursor short-circuit lives
        // there with them.
        let text = self
            .documents
            .text_for_range(&params.text_document.uri, &path, params.range);

        self.tracker.update(Selection {
            path,
            uri: params.text_document.uri,
            range: params.range,
            text,
        });

        // No actions. Zed shows the code-action indicator whenever any server
        // returns one, and the "Explain with Claude" entry this used to return had
        // no command behind it, so it lit up every line of every file and did
        // nothing when chosen. Measured live: Zed keeps issuing codeAction on every
        // cursor move after receiving an empty list, so selection tracking is
        // unaffected.
        Reply::EMPTY_ACTIONS
    }
}

/// Decode a request's `params`, logging and dropping the message if it does not
/// fit. Zed is the only client, so a shape mismatch is a bug on one side or the
/// other and there is nothing useful to do but say so and carry on.
fn decode<T: serde::de::DeserializeOwned>(request: &Request<'_>) -> Option<T> {
    let raw: &RawValue = request.params?;
    match serde_json::from_str(raw.get()) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("params for {} did not decode: {e}", request.method);
            None
        }
    }
}

/// Absolute filesystem path for a document URI, decoding percent-escapes.
/// Falls back to the raw path for non-file schemes.
fn uri_to_path(uri: &Url) -> std::path::PathBuf {
    uri.to_file_path()
        .unwrap_or_else(|_| std::path::PathBuf::from(uri.path()))
}
