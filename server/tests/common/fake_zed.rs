//! Plays Zed's part: speaks LSP to the companion over an in-memory pipe.
//!
//! Frames are `Content-Length: N\r\n\r\n<json>`. Params are built from the real
//! `lsp_types` structs so a shape mistake is a compile error, not a silent drop.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

use lsp_types::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

use claude_code_connect::lsp::serve_lsp;
use claude_code_connect::selection::SelectionTracker;

pub struct FakeZed {
    /// Our end of the pipe the server reads as stdin. Dropping it is EOF there.
    to_server: Option<std::io::PipeWriter>,
    from_server: mpsc::UnboundedReceiver<Value>,
    next_id: i64,
    /// Resolves once the server's LSP thread has returned.
    lsp: tokio::sync::oneshot::Receiver<()>,
}

impl FakeZed {
    /// Start the companion's LSP side on a real OS pipe and attach to it.
    ///
    /// Real pipes rather than an in-memory async duplex, because the server side
    /// is a blocking loop on its own thread -- which is the point of the design,
    /// and what a test that faked the streams would stop exercising.
    pub fn start(tracker: SelectionTracker) -> FakeZed {
        // Two one-way pipes rather than one duplex: dropping our writer must
        // reach the server as EOF.
        let (server_stdin, to_server) = std::io::pipe().expect("pipe");
        let (from_server_raw, server_stdout) = std::io::pipe().expect("pipe");

        let (done, lsp) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            serve_lsp(BufReader::new(server_stdin), server_stdout, tracker);
            let _ = done.send(());
        });

        // Drain everything the server writes from the first byte, on a thread of
        // its own: a full pipe would otherwise block the server mid-response.
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(from_server_raw);
            while let Some(frame) = read_frame(&mut reader) {
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });

        FakeZed {
            to_server: Some(to_server),
            from_server: rx,
            next_id: 0,
            lsp,
        }
    }

    /// `initialize` then `initialized`, as Zed does. Returns the server capabilities.
    pub async fn initialize(&mut self, root: &Path) -> Value {
        let root_uri = Url::from_file_path(root).unwrap();
        let result = self
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {},
                    "workspaceFolders": [{"uri": root_uri, "name": "test"}],
                }),
            )
            .await;
        self.notify("initialized", json!({})).await;
        result["capabilities"].clone()
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let frame = timeout(Duration::from_secs(5), self.from_server.recv())
                .await
                .unwrap_or_else(|_| panic!("no response to {method} within 5s"))
                .expect("server output ended");
            if frame["id"] == json!(id) {
                assert!(
                    frame.get("error").is_none(),
                    "{method} returned an error: {frame}"
                );
                return frame["result"].clone();
            }
            // A server-to-client message (log, progress); not what we are waiting for.
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    pub async fn open(&mut self, uri: &Url, text: &str) {
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "plaintext".to_string(),
                version: 1,
                text: text.to_string(),
            },
        };
        self.notify(
            "textDocument/didOpen",
            serde_json::to_value(params).unwrap(),
        )
        .await;
    }

    pub async fn change(
        &mut self,
        uri: &Url,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) {
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            content_changes: changes,
        };
        self.notify(
            "textDocument/didChange",
            serde_json::to_value(params).unwrap(),
        )
        .await;
    }

    /// What Zed does on every cursor move: ask for code actions over the selection.
    pub async fn select(&mut self, uri: &Url, range: Range) -> Value {
        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            context: CodeActionContext::default(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.request(
            "textDocument/codeAction",
            serde_json::to_value(params).unwrap(),
        )
        .await
    }

    /// Close the pipe, as Zed exiting closes the server's stdin. Waits for the
    /// server's LSP thread to end.
    pub async fn close(mut self) {
        drop(self.to_server.take());
        timeout(Duration::from_secs(5), self.lsp)
            .await
            .expect("LSP thread did not end after the input closed")
            .unwrap();
    }

    async fn write(&mut self, v: Value) {
        let body = v.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut w = self.to_server.take().expect("the pipe is still open");
        // On a blocking thread: a frame larger than the pipe buffer parks until
        // the server drains it, and parking the test runtime would deadlock the
        // one test that opens an oversized document.
        let w = tokio::task::spawn_blocking(move || {
            w.write_all(frame.as_bytes()).unwrap();
            w.flush().unwrap();
            w
        })
        .await
        .unwrap();
        self.to_server = Some(w);
    }
}

fn read_frame(r: &mut BufReader<std::io::PipeReader>) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(n) = line.strip_prefix("Content-Length:") {
            content_length = Some(n.trim().parse().ok()?);
        }
    }
    let mut body = vec![0u8; content_length?];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}
