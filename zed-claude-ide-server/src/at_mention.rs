//! The client side of the `@`-mention hotkey.
//!
//! A Zed keybinding cannot reach the companion directly -- extensions cannot
//! register commands -- but a Zed task can run this binary, which connects to the
//! companion over its own authenticated socket and asks it to mention whatever is
//! selected. See docs/at-mentions.md for the task and keymap.

use std::time::Duration;

use anyhow::Result;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::info;

use crate::discovery::IdeLock;

/// Ask the companion behind `lock` to mention its current selection.
///
/// No range is sent: the companion tracks the live selection already, which
/// sidesteps the question of whether Zed's ZED_ROW is the anchor or the head.
pub async fn send_at_mention_request(lock: &IdeLock) -> Result<()> {
    info!("at-mention via companion on port {}", lock.port);

    let mut request = format!("ws://127.0.0.1:{}", lock.port).into_client_request()?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", "mcp".parse()?);
    request
        .headers_mut()
        .insert("X-Claude-Code-Ide-Authorization", lock.auth_token.parse()?);

    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
    ws.send(Message::Text(
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "at_mention_request",
        })
        .to_string(),
    ))
    .await?;

    // Let the frame reach the server before the socket closes.
    ws.flush().await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = ws.close(None).await;

    Ok(())
}
