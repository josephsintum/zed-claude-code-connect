//! Drives the companion the way the real `claude` CLI does.
//!
//! Every assertion here mirrors behaviour observed in the shipped CLI binary
//! (v2.1.269): the `mcp` subprotocol offer, the `X-Claude-Code-Ide-Authorization`
//! header, an `initialize` request, an `ide_connected` notification, `tools/list`.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;

use claude_code_connect::companion::{Companion, Handle};
use claude_code_connect::config::Config;
use claude_code_connect::lockfile::LockDir;
use claude_code_connect::selection::Selection;

/// A running companion plus everything a client needs to reach it. Holding the
/// handle keeps it running; dropping it at the end of the test stops it.
struct Harness {
    port: u16,
    token: String,
    _handle: Handle,
}

/// One throwaway lock directory for the whole test binary, handed to each server
/// through its config. Ports differ, so lock names never collide.
fn ide_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn cfg(worktree: std::path::PathBuf) -> Config {
    Config::new(worktree, LockDir::at(ide_dir()))
}

/// A selection the editor side would have produced, for injecting into the bus.
fn sel_at(path: &str, text: &str, start: (u32, u32), end: (u32, u32)) -> Selection {
    use lsp_types::{Position, Range, Url};
    Selection {
        path: std::path::PathBuf::from(path),
        uri: Url::from_file_path(path).unwrap(),
        range: Range {
            start: Position {
                line: start.0,
                character: start.1,
            },
            end: Position {
                line: end.0,
                character: end.1,
            },
        },
        text: text.to_string(),
    }
}

fn selection(path: &str, text: &str) -> Selection {
    sel_at(path, text, (1, 0), (2, 4))
}

/// Boot a companion against the throwaway lock directory.
async fn start() -> Harness {
    let handle = Companion::start(cfg(ide_dir().to_path_buf()))
        .await
        .expect("companion starts");
    Harness {
        port: handle.port(),
        token: handle.auth_token().to_string(),
        _handle: handle,
    }
}

fn request(
    port: u16,
    token: Option<&str>,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    // No path, no query -- the CLI dials the bare origin.
    let mut req = format!("ws://127.0.0.1:{port}")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", "mcp".parse().unwrap());
    if let Some(t) = token {
        req.headers_mut()
            .insert("X-Claude-Code-Ide-Authorization", t.parse().unwrap());
    }
    req
}

async fn send(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    v: Value,
) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

async fn recv_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        let msg = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("stream closed")
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test]
async fn handshake_echoes_mcp_subprotocol() {
    let h = start().await;
    let (_ws, resp) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .expect("handshake should succeed with a valid token");
    assert_eq!(
        resp.headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("mcp"),
        "server must echo the mcp subprotocol or the CLI will not negotiate"
    );
}

/// RFC 6455 lets the server select only a protocol the client actually offered, and
/// a client that receives one it did not offer must fail the connection. A substring
/// test on the offer list claims `mcp` to anyone whose protocol merely contains those
/// three letters, which presents to them as an unexplained handshake failure.
#[tokio::test]
async fn a_protocol_that_merely_contains_mcp_is_not_echoed() {
    let h = start().await;
    let mut req = format!("ws://127.0.0.1:{}", h.port)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", "x-mcp-v2".parse().unwrap());
    req.headers_mut()
        .insert("X-Claude-Code-Ide-Authorization", h.token.parse().unwrap());

    let (_ws, resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("handshake should still complete");
    assert_eq!(
        resp.headers().get("sec-websocket-protocol"),
        None,
        "x-mcp-v2 is not mcp; echoing it selects a protocol the client never offered"
    );
}

#[tokio::test]
async fn initialize_reports_negotiated_protocol_version() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "claude-code", "version": "2.1.269"}}
        }),
    )
    .await;

    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["id"], json!(1), "response must echo the request id");
    assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
    assert!(resp["result"]["serverInfo"]["name"].is_string());
}

/// The regression test for the defect that made the companion answer a
/// notification: `ide_connected` carries no id and must draw no frame at all.
#[tokio::test]
async fn ide_connected_draws_no_response() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "method": "ide_connected", "params": {"pid": 4321}
        }),
    )
    .await;

    let quiet = timeout(Duration::from_millis(400), ws.next()).await;
    assert!(
        quiet.is_err(),
        "a notification must never be answered, got: {quiet:?}"
    );
}

/// `id: Option<Value>` deserializes an explicit `"id": null` to Some(Value::Null),
/// so the guard has to treat that as absent too.
#[tokio::test]
async fn explicit_null_id_draws_no_response() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "id": null, "method": "ide_connected", "params": {"pid": 1}
        }),
    )
    .await;

    let quiet = timeout(Duration::from_millis(400), ws.next()).await;
    assert!(
        quiet.is_err(),
        "explicit null id is still a notification, got: {quiet:?}"
    );
}

#[tokio::test]
async fn bad_token_is_closed_with_policy_1008() {
    let h = start().await;
    let wrong = uuid::Uuid::new_v4().to_string();
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&wrong)))
        .await
        .expect("upgrade completes; rejection is a close frame, not an HTTP error");

    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("expected a close frame")
        .expect("stream closed without a frame")
        .unwrap();

    match msg {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Policy),
        other => panic!("expected a 1008 close, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_token_is_closed_with_policy_1008() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, None))
        .await
        .unwrap();

    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("expected a close frame")
        .expect("stream closed without a frame")
        .unwrap();

    match msg {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Policy),
        other => panic!("expected a 1008 close, got {other:?}"),
    }
}

#[tokio::test]
async fn unauthorized_client_cannot_call_tools() {
    let h = start().await;
    let wrong = uuid::Uuid::new_v4().to_string();
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&wrong)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;

    // Anything that arrives must be the close, never a tools/list result.
    while let Ok(Some(Ok(msg))) = timeout(Duration::from_millis(500), ws.next()).await {
        if let Message::Text(t) = msg {
            panic!("unauthorized client received a payload: {t}");
        }
    }
}

/// A client that attaches mid-session has missed every selection sent before it
/// connected, so without a replay it knows nothing until the next cursor move.
#[tokio::test]
async fn current_selection_is_replayed_to_a_new_client() {
    let h = start_with_notifications().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());

    // A selection happens before any client is listening.
    bus.publish_selection(sel_at("/tmp/a.rs", "let answer = 42;", (3, 0), (3, 16)));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();

    let replayed = recv_json(&mut ws).await;
    assert_eq!(replayed["method"], "selection_changed");
    assert_eq!(replayed["params"]["text"], "let answer = 42;");
    assert_eq!(replayed["params"]["selection"]["start"]["line"], 3);
}

/// The at-mention hotkey path: a helper process connects over the same
/// authenticated socket and asks for a mention; the CLI receives `at_mentioned`
/// built from the selection the companion is already tracking.
#[tokio::test]
async fn at_mention_request_reaches_the_cli_as_at_mentioned() {
    let h = start_with_notifications().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());

    // The user selects lines 10-14.
    bus.publish_selection(sel_at("/tmp/widget.rs", "selected block", (10, 0), (14, 8)));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The CLI is attached and has consumed the replayed selection.
    let (mut cli, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();
    let replay = recv_json(&mut cli).await;
    assert_eq!(replay["method"], "selection_changed");

    // The keybinding helper fires.
    let (mut helper, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();
    send(
        &mut helper,
        json!({"jsonrpc": "2.0", "method": "at_mention_request"}),
    )
    .await;

    // The CLI sees an at_mentioned carrying the tracked range.
    loop {
        let msg = recv_json(&mut cli).await;
        if msg["method"] == "at_mentioned" {
            assert_eq!(msg["params"]["filePath"], "/tmp/widget.rs");
            assert_eq!(msg["params"]["lineStart"], 10, "0-based on the wire");
            assert_eq!(msg["params"]["lineEnd"], 14);
            break;
        }
    }
}

/// With nothing selected the mention covers the whole file, so the range is
/// omitted rather than sent as 0 -- which the CLI would read as line 1.
#[tokio::test]
async fn an_empty_selection_mentions_the_file_without_a_range() {
    let h = start_with_notifications().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());

    bus.publish_selection(sel_at("/tmp/widget.rs", "", (7, 3), (7, 3)));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut cli, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();
    let _ = recv_json(&mut cli).await; // replayed selection

    let (mut helper, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();
    send(
        &mut helper,
        json!({"jsonrpc": "2.0", "method": "at_mention_request"}),
    )
    .await;

    loop {
        let msg = recv_json(&mut cli).await;
        if msg["method"] == "at_mentioned" {
            assert_eq!(msg["params"]["filePath"], "/tmp/widget.rs");
            // Indexing a missing key also yields Null, so is_null() cannot tell
            // "absent" from "explicitly null" -- and the CLI validates these with an
            // optional number, which rejects null. Assert the keys are truly absent.
            let params = msg["params"].as_object().unwrap();
            assert!(
                !params.contains_key("lineStart"),
                "lineStart must be omitted, not null: {msg}"
            );
            assert!(!params.contains_key("lineEnd"), "lineEnd must be omitted");
            break;
        }
    }
}

/// Empty, and that is the whole point. Checked against the shipped CLI (2.1.274):
/// `getCurrentSelection`, `getLatestSelection` and `getWorkspaceFolders` appear
/// zero times in the binary. Every IDE tool the CLI does call goes through one
/// helper, only ever passed `openDiff`, `close_tab` and `closeAllDiffTabs` --
/// none of which Zed can serve.
///
/// If a future CLI grows a tool this companion can answer, this test is where the
/// decision to add it back gets recorded.
#[tokio::test]
async fn no_tools_are_advertised_because_the_cli_calls_none_of_them() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;

    let resp = loop {
        let msg = recv_json(&mut ws).await;
        if msg["id"] == json!(1) {
            break msg;
        }
    };

    assert_eq!(
        resp["result"]["tools"],
        json!([]),
        "advertising a tool the CLI never calls is surface with no user"
    );
}

/// A companion whose bus a test can publish selections into, as the editor side
/// would. The handle must be kept alive for the companion to keep running.
async fn start_with_notifications() -> Handle {
    Companion::start(cfg(ide_dir().to_path_buf()))
        .await
        .expect("companion starts")
}

/// A companion whose event ring holds two entries, so a client that does not read
/// is overrun in a handful of publishes rather than a hundred-odd. tokio rounds
/// broadcast capacity up to a power of two, which is why the old "publish 80 into
/// a 100-slot ring" flood never lagged at all: the ring was really 128.
async fn start_with_tiny_event_ring() -> Handle {
    Companion::start(cfg(ide_dir().to_path_buf()).with_event_capacity(2))
        .await
        .expect("companion starts")
}

/// The replay a new client gets comes from the `watch` cell, not the broadcast ring:
/// `publish_selection` writes the cell first and the ring is best effort. So a client
/// that connects long after the editor went quiet still learns where the cursor is.
///
/// This test was named for lag and did not test it: it connects only after every
/// publish, so there is no subscriber, nothing enters the ring, and the assertion
/// reads the cell either way. Lag itself is covered by the test below.
#[tokio::test]
async fn the_replay_cache_holds_the_selection_sent_last() {
    let h = start_with_notifications().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());

    // Nothing is connected yet, so none of these reach the broadcast ring at all.
    for i in 0..80 {
        bus.publish_selection(selection("/tmp/burst.rs", &format!("burst {i}")));
    }
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The value that must survive: sent after the burst, so a frozen cache
    // cannot possibly hold it.
    bus.publish_selection(selection("/tmp/after.rs", "after the burst"));
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();

    let replay = recv_json(&mut ws).await;
    assert_eq!(replay["method"], "selection_changed");
    assert_eq!(
        replay["params"]["filePath"], "/tmp/after.rs",
        "cache stopped updating after a lag: {replay}"
    );
}

/// A client that lags must keep receiving once it catches up, rather than being
/// silently unsubscribed while its connection stays open and looks healthy.
#[tokio::test]
async fn a_lagging_client_still_receives_later_notifications() {
    let h = start_with_tiny_event_ring().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());

    let (mut ws, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();

    for i in 0..80 {
        bus.publish_selection(selection("/tmp/flood.rs", &format!("flood {i}")));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    bus.publish_selection(selection("/tmp/final.rs", "the one that matters"));

    // Drain until the post-lag notification arrives; a silently unsubscribed
    // client never sees it and this times out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "never received the notification sent after the lag"
        );
        let msg = recv_json(&mut ws).await;
        if msg["params"]["filePath"] == "/tmp/final.rs" {
            break;
        }
    }
}

/// `id` is mandatory in every JSON-RPC response. A client matches responses to
/// requests by it, so an error that omits it is invisible: the CLI waits out its
/// timeout instead of surfacing the failure.
#[tokio::test]
async fn an_internal_error_echoes_the_request_id() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    // tools/call without a name fails inside the handler.
    send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {}
        }),
    )
    .await;

    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["id"], json!(7), "error must be correlatable: {resp}");
    assert!(resp["error"].is_object());
    assert!(
        resp.as_object().unwrap().contains_key("id"),
        "id must be present, not omitted: {resp}"
    );
}

/// An unknown method is a normal error response and must also carry the id.
#[tokio::test]
async fn an_unknown_method_echoes_the_request_id() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({
            "jsonrpc": "2.0", "id": "abc", "method": "no/such/method"
        }),
    )
    .await;

    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["id"], json!("abc"));
    assert_eq!(resp["error"]["code"], json!(-32601));
}

/// A caller that omits `name`, or `params` entirely, has sent bad arguments -- not
/// provoked a fault in this server. JSON-RPC separates the two, and -32603 tells a
/// client to retry or report a bug where -32602 tells it to fix the call.
#[tokio::test]
async fn tools_call_without_a_name_is_invalid_params_not_an_internal_error() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}}),
    )
    .await;
    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["id"], json!(1));
    assert_eq!(resp["error"]["code"], json!(-32602), "missing tool name");

    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call"}),
    )
    .await;
    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["id"], json!(2));
    assert_eq!(resp["error"]["code"], json!(-32602), "missing params");
}

/// The CLI calls `openDiff` itself -- not through the model -- and on a reply it
/// judges successful it takes `content[1].text` as the new file contents and
/// reports `saved: true`, meaning a human reviewed and accepted the edit. A
/// refusal dressed as a success is the one shape that must never leave here.
///
/// `getDiagnostics` is the opposite case: unadvertised, but a real answer.
#[tokio::test]
async fn a_refused_tool_is_flagged_as_an_error_and_an_answered_one_is_not() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    for (id, tool) in [(1, "openDiff"), (2, "close_tab"), (3, "closeAllDiffTabs")] {
        send(
            &mut ws,
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                   "params": {"name": tool, "arguments": {}}}),
        )
        .await;
        let resp = recv_json(&mut ws).await;
        assert_eq!(
            resp["result"]["isError"],
            json!(true),
            "{tool} was refused, so the reply must not claim success: {resp}"
        );
    }

    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "getDiagnostics", "arguments": {}}}),
    )
    .await;
    let resp = recv_json(&mut ws).await;
    assert_eq!(
        resp["result"]["isError"],
        json!(false),
        "getDiagnostics really is answered: {resp}"
    );
}

/// Text that is not JSON leaves no id to answer with. The spec requires null in
/// that case -- and null specifically, not an absent key.
#[tokio::test]
async fn unparseable_text_draws_a_parse_error_with_a_null_id() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    ws.send(Message::Text("{not json at all".to_string()))
        .await
        .unwrap();

    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["error"]["code"], json!(-32700));
    assert!(
        resp.as_object().unwrap().contains_key("id"),
        "id is mandatory even when null: {resp}"
    );
    assert!(resp["id"].is_null());
}

/// Valid JSON of the wrong shape still carries a usable id, which is exactly when
/// returning an error beats dropping the message.
#[tokio::test]
async fn a_malformed_request_with_an_id_draws_invalid_request() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    // Parses as JSON; `method` is not a string, so it is not an MCPRequest.
    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 42, "method": 12345}),
    )
    .await;

    let resp = recv_json(&mut ws).await;
    assert_eq!(resp["id"], json!(42));
    assert_eq!(resp["error"]["code"], json!(-32600));
}

/// A malformed message with no id is indistinguishable from a broken notification,
/// and notifications are never answered.
#[tokio::test]
async fn a_malformed_request_without_an_id_draws_nothing() {
    let h = start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(request(h.port, Some(&h.token)))
        .await
        .unwrap();

    send(&mut ws, json!({"jsonrpc": "2.0", "method": 12345})).await;

    let quiet = timeout(Duration::from_millis(400), ws.next()).await;
    assert!(quiet.is_err(), "must not answer a notification: {quiet:?}");
}

/// The exact key set of `selection_changed` is what the CLI parses. Pinned before
/// the internal representation changes, so a refactor cannot move a key without
/// this saying so.
#[tokio::test]
async fn selection_changed_params_have_exactly_the_keys_the_cli_parses() {
    let h = start_with_notifications().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());
    let (mut ws, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();

    bus.publish_selection(selection("/tmp/pin.rs", "pinned"));

    let msg = loop {
        let msg = recv_json(&mut ws).await;
        if msg["method"] == "selection_changed" {
            break msg;
        }
    };
    let params = msg["params"].as_object().unwrap();
    let mut keys: Vec<&str> = params.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["filePath", "fileUrl", "selection", "text"]);

    let selection = params["selection"].as_object().unwrap();
    let mut keys: Vec<&str> = selection.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["end", "isEmpty", "start"]);

    for end in ["start", "end"] {
        let mut keys: Vec<&str> = selection[end]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["character", "line"], "{end}");
    }
    assert_eq!(params["fileUrl"], "file:///tmp/pin.rs");
}

/// Likewise for `at_mentioned`: `filePath` always, `lineStart`/`lineEnd` only for
/// a non-empty selection, nothing else.
#[tokio::test]
async fn at_mentioned_params_have_exactly_the_keys_the_cli_parses() {
    let h = start_with_notifications().await;
    let (port, token, bus) = (h.port(), h.auth_token().to_string(), h.bus().clone());

    bus.publish_selection(selection("/tmp/pin.rs", "pinned"));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut cli, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();
    let _ = recv_json(&mut cli).await; // replay

    let (mut helper, _) = tokio_tungstenite::connect_async(request(port, Some(&token)))
        .await
        .unwrap();
    send(
        &mut helper,
        json!({"jsonrpc": "2.0", "method": "at_mention_request"}),
    )
    .await;

    let msg = loop {
        let msg = recv_json(&mut cli).await;
        if msg["method"] == "at_mentioned" {
            break msg;
        }
    };
    let mut keys: Vec<&str> = msg["params"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["filePath", "lineEnd", "lineStart"]);
    assert_eq!(msg["params"]["lineStart"], 1);
    assert_eq!(msg["params"]["lineEnd"], 2);
}
