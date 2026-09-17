//! Connects to a running companion exactly as the `claude` CLI does and prints
//! every notification it pushes, with timestamps.
//!
//! Discovery mirrors the CLI: scan the lock directory, pick the lock whose
//! workspaceFolders contain the given path, read its port and token.
//!
//!     cargo run -p zed-claude-ide-server --example watch -- /path/to/project
//!
//! With no argument it lists the locks it can see.

use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

fn stamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        now.subsec_millis()
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = std::env::args().nth(1);
    let dir = zed_claude_ide_server::lockfile::LockDir::resolve(
        std::env::var_os("ZED_CLAUDE_IDE_DIR"),
        std::env::var_os("CLAUDE_CONFIG_DIR"),
    )?;

    let Some(target) = target else {
        // No argument: list what is discoverable, as the CLI would see it.
        for lock in zed_claude_ide_server::discovery::all_locks(&dir)? {
            println!(
                "port {:<6} {:<22} {}",
                lock.port,
                lock.ide_name,
                lock.workspace_folders.join(", ")
            );
        }
        println!("\nPass a project path to attach to one of the above.");
        return Ok(());
    };

    let lock = zed_claude_ide_server::discovery::lock_for(&dir, std::path::Path::new(&target))?;
    let (port, token, folder) = (
        lock.port,
        lock.auth_token.clone(),
        lock.workspace_folders.join(", "),
    );

    println!("attaching to port {port} ({folder})\n");

    let mut req = format!("ws://127.0.0.1:{port}").into_client_request()?;
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", "mcp".parse()?);
    req.headers_mut()
        .insert("X-Claude-Code-Ide-Authorization", token.parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;

    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "watch", "version": "0"}}
        })
        .to_string(),
    ))
    .await?;
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0", "method": "ide_connected", "params": {"pid": std::process::id()}
        })
        .to_string(),
    ))
    .await?;

    println!("connected. move the cursor and select text in Zed; ctrl-c to stop.\n");

    let mut last = std::time::Instant::now();
    while let Some(msg) = ws.next().await {
        let Message::Text(t) = msg? else { continue };
        let v: Value = serde_json::from_str(&t)?;
        let Some(method) = v["method"].as_str() else {
            // Slice by characters: byte-slicing panics when the cut lands inside a
            // multi-byte sequence, and non-ASCII is normal in these payloads.
            let preview: String = t.chars().take(120).collect();
            println!("{}  [response] {}", stamp(), preview);
            continue;
        };
        let gap = last.elapsed();
        last = std::time::Instant::now();

        if method == "selection_changed" {
            let p = &v["params"];
            let sel = &p["selection"];
            let text = p["text"].as_str().unwrap_or("");
            let preview: String = text.chars().take(48).collect();
            println!(
                "{}  (+{:>5}ms) selection_changed  {}:{}-{}:{}  {} chars  {:?}{}",
                stamp(),
                gap.as_millis(),
                sel["start"]["line"],
                sel["start"]["character"],
                sel["end"]["line"],
                sel["end"]["character"],
                text.chars().count(),
                preview,
                if text.chars().count() > 48 { "..." } else { "" },
            );
        } else {
            println!(
                "{}  (+{:>5}ms) {}  {}",
                stamp(),
                gap.as_millis(),
                method,
                v["params"]
            );
        }
    }
    Ok(())
}
