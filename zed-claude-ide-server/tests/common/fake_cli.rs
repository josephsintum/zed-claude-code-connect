//! Plays the `claude` CLI's part: dials the companion exactly as the shipped
//! binary does (v2.1.269) and speaks MCP over the WebSocket.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub struct FakeCli {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: i64,
}

impl FakeCli {
    pub async fn connect(port: u16, token: &str) -> FakeCli {
        // No path, no query -- the CLI dials the bare origin.
        let mut req = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", "mcp".parse().unwrap());
        req.headers_mut()
            .insert("X-Claude-Code-Ide-Authorization", token.parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        FakeCli { ws, next_id: 0 }
    }

    /// `initialize`, then the `ide_connected` notification, as the CLI does.
    pub async fn handshake(&mut self) -> Value {
        let result = self
            .request(
                "initialize",
                json!({"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "claude-code", "version": "2.1.269"}}),
            )
            .await;
        self.send(json!({"jsonrpc": "2.0", "method": "ide_connected",
                         "params": {"pid": std::process::id()}}))
            .await;
        result
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let v = self.recv(Duration::from_secs(5)).await.expect("response");
            if v["id"] == json!(id) {
                return v["result"].clone();
            }
        }
    }

    /// Call a tool and decode the JSON payload inside its text content block.
    pub async fn call_tool(&mut self, name: &str) -> Value {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": {}}))
            .await;
        let inner = result["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(inner).unwrap()
    }

    /// The next notification with this method, skipping anything else, or None
    /// if nothing arrives within `wait`.
    pub async fn next_notification(&mut self, method: &str, wait: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let v = self.recv(remaining).await?;
            if v["method"] == method {
                return Some(v["params"].clone());
            }
        }
    }

    /// Assert nothing is pushed for `wait`. Responses cannot arrive here because
    /// nothing is outstanding.
    pub async fn expect_quiet(&mut self, wait: Duration) {
        if let Some(v) = self.recv(wait).await {
            panic!("expected no frames, got {v}");
        }
    }

    /// True if the server closes the connection within `wait`; frames that arrive
    /// meanwhile are ignored.
    pub async fn closed_within(&mut self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match timeout(remaining, self.ws.next()).await {
                Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => return true,
                Ok(Some(Ok(_))) => continue,
                Err(_) => return false,
            }
        }
    }

    async fn send(&mut self, v: Value) {
        self.ws.send(Message::Text(v.to_string())).await.unwrap();
    }

    async fn recv(&mut self, wait: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let msg = match timeout(remaining, self.ws.next()).await {
                Ok(Some(Ok(msg))) => msg,
                // Timed out, stream ended, or a transport error: nothing to return.
                _ => return None,
            };
            if let Message::Text(t) = msg {
                return serde_json::from_str(&t).ok();
            }
        }
    }
}
