//! One CLI connection: the auth handshake, then a loop that answers MCP requests
//! and pushes editor events until the client leaves or the companion stops.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use subtle::ConstantTimeEq;
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
    tungstenite::protocol::frame::coding::CloseCode,
    tungstenite::protocol::CloseFrame,
    tungstenite::Message,
    WebSocketStream,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::protocol::{Dispatcher, MCPError, MCPRequest, MCPResponse};
use super::wire;
use crate::selection::{EventBus, Mention};

pub(super) async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    auth_token: String,
    bus: EventBus,
    worktree: PathBuf,
    cancel: CancellationToken,
) -> Result<()> {
    // The handshake callback is the only place the request headers are visible, so
    // stash the token here and check it once the upgrade has completed. The protocol
    // requires rejection via a 1008 close frame, which only exists post-handshake.
    let presented: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let presented_cb = Arc::clone(&presented);

    // The Err variant here is tungstenite's ErrorResponse; its size is fixed by the
    // API being called, and this runs once per connection.
    #[allow(clippy::result_large_err)]
    let ws_stream = match accept_hdr_async(stream, move |req: &Request, mut response: Response| {
        // HeaderMap lookup is case-insensitive; the CLI sends this capitalised.
        *presented_cb.lock().unwrap() = req
            .headers()
            .get("x-claude-code-ide-authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        // Check if client requested MCP protocol
        if let Some(protocols) = req.headers().get("Sec-WebSocket-Protocol") {
            if let Ok(protocols_str) = protocols.to_str() {
                if protocols_str.contains("mcp") {
                    // Add MCP protocol to response
                    response
                        .headers_mut()
                        .insert("Sec-WebSocket-Protocol", "mcp".parse().unwrap());
                    debug!("MCP protocol negotiated for {}", peer_addr);
                }
            }
        }
        Ok(response)
    })
    .await
    {
        Ok(ws) => {
            debug!("WebSocket handshake completed for {}", peer_addr);
            ws
        }
        Err(e) => {
            error!("WebSocket handshake failed for {}: {}", peer_addr, e);
            return Err(e.into());
        }
    };

    let authorized = {
        let guard = presented.lock().unwrap();
        match guard.as_deref() {
            // Equal-length check first: ct_eq is only defined for equal-length slices,
            // and the token length is not a secret.
            Some(t) => {
                t.len() == auth_token.len() && bool::from(t.as_bytes().ct_eq(auth_token.as_bytes()))
            }
            None => false,
        }
    };

    if !authorized {
        warn!(
            "Rejecting {}: missing or invalid X-Claude-Code-Ide-Authorization",
            peer_addr
        );
        let mut ws_stream = ws_stream;
        let _ = ws_stream
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Policy, // 1008
                reason: "invalid auth token".into(),
            })))
            .await;
        let _ = ws_stream.close(None).await;
        return Ok(());
    }

    debug!("Authorized WebSocket connection from {}", peer_addr);
    handle_websocket_connection(ws_stream, peer_addr, bus, worktree, cancel).await
}

async fn handle_websocket_connection(
    ws_stream: WebSocketStream<TcpStream>,
    peer_addr: SocketAddr,
    bus: EventBus,
    worktree: PathBuf,
    cancel: CancellationToken,
) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Subscribe first, then read `latest` for the replay. In that order a
    // selection published in between is seen twice, which the CLI tolerates; in
    // the other order it would be seen never.
    let mut events = Some(bus.subscribe());
    let mcp_handler = Dispatcher::new(worktree, bus.clone());

    info!("WebSocket connection established with {}", peer_addr);

    // A client that connects mid-session has missed every selection sent so far,
    // so it would know nothing until the user next moved the cursor. Replay the
    // current one, as the VS Code extension does.
    if let Some(selection) = bus.latest() {
        debug!("Replaying current selection to {}", peer_addr);
        let frame = serde_json::to_string(&wire::selection_changed(&selection))?;
        let _ = ws_sender.send(Message::Text(frame)).await;
    }

    let mut lagged_before = false;

    // Main message loop handling both WebSocket messages and editor events
    loop {
        tokio::select! {
            // The companion is shutting down: say so, rather than vanish.
            _ = cancel.cancelled() => {
                info!("Closing connection with {}: shutting down", peer_addr);
                let _ = ws_sender
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Away, // 1001
                        reason: "shutting down".into(),
                    })))
                    .await;
                let _ = ws_sender.close().await;
                break;
            }
            // Handle incoming WebSocket messages
            msg = ws_receiver.next() => {
                match msg {
                    Some(msg) => {
                        if let Err(e) = handle_websocket_message(
                            msg,
                            &mcp_handler,
                            &mut ws_sender,
                            peer_addr,
                            &bus,
                        )
                        .await
                        {
                            error!("Error handling WebSocket message: {}", e);
                            break;
                        }
                    }
                    None => {
                        info!("WebSocket connection with {} ended", peer_addr);
                        break;
                    }
                }
            },
            // Forward editor events
            event = async {
                match events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Ok(event) => {
                        let frame = match serde_json::to_string(&wire::notification_for(&event)) {
                            Ok(frame) => frame,
                            Err(e) => {
                                // This one event's problem, not the connection's.
                                error!("Dropping unserializable event: {}", e);
                                continue;
                            }
                        };
                        debug!("Pushing {:?} to {}", event, peer_addr);
                        if let Err(e) = ws_sender.send(Message::Text(frame)).await {
                            error!("Failed to push event to {}: {}", peer_addr, e);
                            break;
                        }
                    }
                    // Lagging does not close the channel: the cursor advances to the
                    // oldest retained event and the next recv succeeds. Only the
                    // newest selection matters, so what was skipped is not missed.
                    Err(RecvError::Lagged(skipped)) => {
                        if lagged_before {
                            debug!("{} fell behind again, skipped {} event(s)", peer_addr, skipped);
                        } else {
                            warn!("{} fell behind, skipped {} event(s)", peer_addr, skipped);
                            lagged_before = true;
                        }
                    }
                    // The bus is gone: shutdown. Keep serving tools/call until the
                    // client leaves; the pending() branch above parks this arm.
                    Err(RecvError::Closed) => {
                        debug!("Event bus closed for {}", peer_addr);
                        events = None;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_websocket_message(
    msg: Result<Message, tokio_tungstenite::tungstenite::Error>,
    mcp_handler: &Dispatcher,
    ws_sender: &mut futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
    peer_addr: SocketAddr,
    bus: &EventBus,
) -> Result<()> {
    match msg {
        Ok(msg) => {
            if msg.is_text() {
                let text = msg.to_text().unwrap();
                debug!("Received message from {}: {}", peer_addr, text);

                // Parse as generic JSON first. A message can be valid JSON but the
                // wrong shape -- a missing `jsonrpc`, a non-string `method` -- and in
                // that case the id is still there to answer with, which is the whole
                // point of returning an error rather than dropping it.
                let raw = match serde_json::from_str::<serde_json::Value>(text) {
                    Ok(value) => value,
                    Err(e) => {
                        warn!("Unparseable message from {}: {}", peer_addr, e);
                        debug!("Invalid message content: {}", text);
                        // No id is recoverable from text that is not JSON, and the
                        // spec requires null rather than omission in that case.
                        return send_error(
                            ws_sender,
                            peer_addr,
                            Some(serde_json::Value::Null),
                            -32700,
                            "Parse error",
                            None,
                        )
                        .await;
                    }
                };

                let recovered_id = raw.get("id").cloned();

                match serde_json::from_value::<MCPRequest>(raw) {
                    Ok(mcp_request) => {
                        debug!("Processing MCP request: {}", mcp_request.method);

                        // A JSON-RPC notification carries no id and must never be
                        // answered. The CLI sends `ide_connected` this way, which the
                        // old `notifications/` prefix test missed -- it fell through to
                        // the request path and drew a -32601 back.
                        //
                        // `id: Option<Value>` means an explicit `"id": null` arrives as
                        // Some(Value::Null), so both shapes count as absent.
                        if matches!(mcp_request.id, None | Some(serde_json::Value::Null)) {
                            debug!("Received notification: {}", mcp_request.method);
                            if mcp_request.method == "at_mention_request" {
                                handle_at_mention_request(bus);
                            }
                            return Ok(());
                        }

                        // handle_request consumes the request, so keep the id for
                        // the error path.
                        let request_id = mcp_request.id.clone();

                        match mcp_handler.handle_request(mcp_request).await {
                            Ok(response) => {
                                let response_json = serde_json::to_string(&response)?;
                                debug!("Sending MCP response: {}", response_json);

                                if let Err(e) = ws_sender.send(Message::Text(response_json)).await {
                                    error!("Failed to send MCP response to {}: {}", peer_addr, e);
                                    return Err(e.into());
                                }
                            }
                            Err(e) => {
                                error!("Error handling MCP request: {}", e);
                                return send_error(
                                    ws_sender,
                                    peer_addr,
                                    request_id,
                                    -32603,
                                    "Internal error",
                                    Some(serde_json::json!({"details": e.to_string()})),
                                )
                                .await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Malformed request from {}: {}", peer_addr, e);
                        debug!("Invalid message content: {}", text);

                        // Valid JSON, wrong shape. Answer only when an id was
                        // recoverable: without one this is indistinguishable from a
                        // malformed notification, and notifications are never answered.
                        match recovered_id {
                            Some(id) if !id.is_null() => {
                                return send_error(
                                    ws_sender,
                                    peer_addr,
                                    Some(id),
                                    -32600,
                                    "Invalid Request",
                                    None,
                                )
                                .await;
                            }
                            _ => return Ok(()),
                        }
                    }
                }
            } else if msg.is_close() {
                debug!("Connection closed by {}", peer_addr);
                return Ok(());
            }
        }
        Err(e) => {
            error!("WebSocket error for {}: {}", peer_addr, e);
            return Err(e.into());
        }
    }

    Ok(())
}

/// Turn the current selection into an `at_mentioned` event.
///
/// A Zed keybinding cannot reach this process directly -- extensions cannot
/// register commands -- so the helper connects over this same authenticated
/// socket and sends `at_mention_request`. It carries no range: the companion
/// already tracks the live selection, which avoids having to work out whether
/// Zed's ZED_ROW refers to the anchor or the head of a selection. An empty
/// selection mentions the whole file.
fn handle_at_mention_request(bus: &EventBus) {
    let Some(selection) = bus.latest() else {
        warn!("at_mention_request received before any selection was seen");
        return;
    };
    let mention = Mention::of(&selection);
    info!(
        "at-mention: {} lines {:?}",
        mention.path.display(),
        mention.lines
    );
    bus.publish_mention(mention);
}

async fn send_error(
    ws_sender: &mut futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
    peer_addr: SocketAddr,
    id: Option<serde_json::Value>,
    code: i32,
    message: &str,
    data: Option<serde_json::Value>,
) -> Result<()> {
    let response = MCPResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(MCPError {
            code,
            message: message.to_string(),
            data,
        }),
    };

    let json = serde_json::to_string(&response)?;
    if let Err(e) = ws_sender.send(Message::Text(json)).await {
        error!("Failed to send error response to {}: {}", peer_addr, e);
        return Err(e.into());
    }
    Ok(())
}
