//! The editor side: what Zed sends over LSP, and what is done with it.
//!
//! This is not a language server. It attaches to every buffer only to observe
//! the active file and selection, so it advertises document sync (for the buffer
//! mirror) and code actions (the one request Zed sends with the live selection
//! range), and nothing else.

use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tower_service::Service;
use tracing::{debug, info};

use super::documents::DocumentStore;
use crate::selection::{Selection, SelectionTracker};

#[derive(Debug)]
pub struct ClaudeCodeLanguageServer {
    /// Where every selection Zed reports goes; debouncing happens there.
    tracker: SelectionTracker,
    /// Mirror of the buffers Zed has open, so selection text reflects unsaved edits.
    documents: Arc<DocumentStore>,
}

impl ClaudeCodeLanguageServer {
    /// The `Client` tower-lsp hands over is unused: this server never sends
    /// anything to Zed on its own initiative, and stderr already reaches Zed's
    /// LSP log panel.
    pub fn new(_client: Client, tracker: SelectionTracker) -> Self {
        Self {
            tracker,
            documents: Arc::new(DocumentStore::new()),
        }
    }
}

/// Serve LSP over any pair of byte streams until the input ends or the client
/// sends `exit`.
///
/// The binary passes stdio. The tests pass an in-memory pipe and play Zed's
/// part, which is the only way to exercise didOpen -> codeAction ->
/// selection_changed without an editor.
///
/// `exit` matters because it is how Zed actually ends a language server, on
/// quit and on restart alike: `shutdown`, then `exit`, then it kills the
/// process. It does not close stdin first. tower-lsp records the exit but keeps
/// reading until EOF, so left to itself this would sit until the kill arrived
/// and the lock file would outlive the process. Observed live: every restart
/// left a dead "Zed" entry in the CLI's /ide picker.
pub async fn serve_lsp<I, O>(input: I, output: O, tracker: SelectionTracker)
where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    let (service, socket) =
        LspService::new(|client| ClaudeCodeLanguageServer::new(client, tracker.clone()));
    let exited = CancellationToken::new();
    let service = ExitWatch {
        inner: service,
        exited: exited.clone(),
    };
    // One handler at a time. tower-lsp's default is four, which orders two
    // didChange notifications for one buffer only by the accident that each
    // completes on its first poll. Every handler here is synchronous once it
    // has its request, so serialising costs nothing and makes document sync
    // ordering a guarantee.
    let server = Server::new(input, output, socket)
        .concurrency_level(1)
        .serve(service);
    tokio::select! {
        _ = server => info!("LSP input ended"),
        _ = exited.cancelled() => info!("LSP client sent exit"),
    }
}

/// Passes every request through to tower-lsp and notes when `exit` goes by.
///
/// The client sends `exit` only after it has received the `shutdown` response,
/// so nothing is left to flush when this fires.
struct ExitWatch<S> {
    inner: S,
    exited: CancellationToken,
}

impl<S> Service<Request> for ExitWatch<S>
where
    S: Service<Request, Response = Option<Response>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        if request.method() == "exit" {
            self.exited.cancel();
        }
        self.inner.call(request)
    }
}

/// Absolute filesystem path for a document URI, decoding percent-escapes.
/// Falls back to the raw path for non-file schemes.
fn uri_to_path(uri: &tower_lsp::lsp_types::Url) -> std::path::PathBuf {
    uri.to_file_path()
        .unwrap_or_else(|_| std::path::PathBuf::from(uri.path()))
}

#[tower_lsp::async_trait]
impl LanguageServer for ClaudeCodeLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        info!("LSP server initializing");
        // Everything Zed told us about itself, for when a capability question
        // comes up. Too much for the normal log.
        debug!("client capabilities: {:?}", params.capabilities);
        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                debug!("workspace folder: {}", folder.uri);
            }
        }

        Ok(InitializeResult {
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
                hover_provider: None,
                completion_provider: None,
                definition_provider: None,
                references_provider: None,
                document_symbol_provider: None,
                workspace_symbol_provider: None,
                // Zed's own tree-sitter expansion beats anything a parser-less
                // server could return.
                selection_range_provider: None,
                // Load-bearing: Zed passes the live selection range on these, which
                // is what drives selection_changed.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "Claude Code Language Server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("LSP server initialized");
    }

    async fn shutdown(&self) -> LspResult<()> {
        info!("LSP server shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("Document opened: {}", params.text_document.uri);

        self.documents
            .open(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        debug!("Document changed: {}", params.text_document.uri);

        self.documents
            .apply(&params.text_document.uri, params.content_changes);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        debug!("Document saved: {}", params.text_document.uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("Document closed: {}", params.text_document.uri);

        self.documents.close(&params.text_document.uri);
    }

    async fn code_action(&self, params: CodeActionParams) -> LspResult<Option<CodeActionResponse>> {
        debug!("Code action requested for range: {:?}", params.range);

        // `Url::path()` is percent-encoded, so a project path containing a space
        // arrives as `/Users/me/My%20Project/a.rs` -- unopenable, and meaningless to
        // the CLI. `to_file_path()` gives the real path.
        let path = uri_to_path(&params.text_document.uri);
        // The mirror reflects unsaved edits; disk is the fallback for documents
        // too large to mirror, or never opened through document sync. Both go
        // through the same range code, so a position means the same thing
        // whichever source answered.
        let text = self
            .documents
            .text_in_range(&params.text_document.uri, params.range)
            .unwrap_or_else(|| DocumentStore::text_from_disk(&path, params.range));

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
        Ok(Some(Vec::new()))
    }

    /// Zed asks for syntax-aware selection expansion here. This server has no
    /// parser, and the previous stub answered with a synthetic one-character range
    /// at each position -- which it also pushed out as `selection_changed`, so any
    /// cursor movement overwrote the real selection captured by `code_action`.
    ///
    /// Returning None lets Zed fall back to its own tree-sitter expansion.
    async fn selection_range(
        &self,
        _params: SelectionRangeParams,
    ) -> LspResult<Option<Vec<SelectionRange>>> {
        Ok(None)
    }
}
