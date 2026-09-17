//! MCP over JSON-RPC 2.0: the envelope types and the request dispatcher, plus
//! the handful of tools this companion answers.
//!
//! The CLI filters the IDE's tool list before the model sees it, keeping only
//! `executeCode` and `getDiagnostics`. This server advertises neither -- there is
//! no Jupyter kernel, and Zed does not forward other language servers'
//! diagnostics here -- so the model sees no IDE tools from us at all. The tools
//! below are what the CLI itself may call.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

#[derive(Debug, Serialize, Deserialize)]
pub struct MCPRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MCPResponse {
    pub jsonrpc: String,
    /// Mandatory in every response, and null only when the request's id could not
    /// be determined. Deliberately not skipped when None: omitting it leaves the
    /// client unable to match the response to its request, so it waits out a
    /// timeout instead of seeing the error.
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<MCPError>,
}

/// A handler rejecting the arguments it was given, rather than failing at them.
///
/// Every other handler error becomes -32603 Internal error, which tells a client to
/// retry or report a bug. Bad arguments are -32602: the call itself is what needs
/// fixing. `connection.rs` downcasts to tell them apart.
#[derive(Debug)]
pub struct InvalidParams(pub String);

impl std::fmt::Display for InvalidParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidParams {}

#[derive(Debug, Serialize, Deserialize)]
pub struct MCPError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub tools: Option<ToolsCapability>,
    pub prompts: Option<PromptsCapability>,
    pub logging: Option<LoggingCapability>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptsCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoggingCapability {}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
}

/// Answers one connection's MCP requests.
///
/// Holds no selection state of its own: the tools read the bus's `latest`,
/// which is the same value every connection is sent, so a client that connects
/// mid-session can answer for the selection it never saw arrive.
/// Answers the MCP request half of a connection. It holds no editor state: with
/// no tools left to serve, every request is answered from the protocol alone.
pub struct Dispatcher {
    pub(crate) capabilities: ServerCapabilities,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            capabilities: create_capabilities(),
        }
    }
}

impl Dispatcher {
    pub async fn handle_request(&self, request: MCPRequest) -> Result<MCPResponse> {
        debug!("Handling MCP request: {}", request.method);
        debug!("Request params: {:?}", request.params);

        let result = match request.method.as_str() {
            "initialize" => self.handle_initialize(request.params).await?,
            "tools/list" => self.handle_tools_list().await?,
            "tools/call" => self.handle_tools_call(request.params).await?,
            "logging/setLevel" => self.handle_logging_set_level(request.params).await?,
            "prompts/list" => self.handle_prompts_list().await?,
            "prompts/get" => self.handle_prompts_get(request.params).await?,
            _ => {
                return Ok(MCPResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: None,
                    error: Some(MCPError {
                        code: -32601,
                        message: format!("Method not found: {}", request.method),
                        data: None,
                    }),
                });
            }
        };

        Ok(MCPResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id,
            result: Some(result),
            error: None,
        })
    }

    async fn handle_initialize(&self, params: Option<Value>) -> Result<Value> {
        debug!("Initializing MCP session");

        if let Some(params) = params {
            debug!("Initialize params: {}", params);
            if let Some(v) = params.get("protocolVersion") {
                debug!("Client requested protocolVersion {}", v);
            }
        }

        Ok(serde_json::json!({
            "protocolVersion": "2025-03-26",
            "capabilities": self.capabilities,
            "serverInfo": ServerInfo {
                name: "Claude Code Zed MCP".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string()
            }
        }))
    }

    /// Nothing, deliberately. Verified against the shipped CLI (2.1.274):
    /// `getCurrentSelection`, `getLatestSelection` and `getWorkspaceFolders`
    /// appear zero times in the binary, so it never calls them. Every IDE tool the
    /// CLI does invoke goes through one helper, and that helper is only ever
    /// passed `openDiff`, `close_tab` and `closeAllDiffTabs` -- none of which Zed
    /// can serve (architecture.md §11).
    ///
    /// The feature was never the tools. It is `selection_changed` and
    /// `at_mentioned`, which are notifications.
    async fn handle_tools_list(&self) -> Result<Value> {
        debug!("Listing available tools");
        Ok(serde_json::json!({ "tools": [] }))
    }

    async fn handle_tools_call(&self, params: Option<Value>) -> Result<Value> {
        let params =
            params.ok_or_else(|| InvalidParams("tools/call requires params".to_string()))?;

        let tool_name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| InvalidParams("tools/call requires a string name".to_string()))?;

        let default_args = serde_json::json!({});
        let arguments = params.get("arguments").unwrap_or(&default_args);

        debug!("Calling tool: {}", tool_name);
        debug!("Tool arguments: {}", arguments);

        let (content, is_error) = dispatch_tool(tool_name);

        Ok(serde_json::json!({
            "content": content,
            "isError": is_error
        }))
    }

    async fn handle_logging_set_level(&self, params: Option<Value>) -> Result<Value> {
        if let Some(params) = params {
            let level = params
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("info");
            debug!("Setting log level to: {}", level);
        }

        Ok(serde_json::json!({}))
    }

    async fn handle_prompts_list(&self) -> Result<Value> {
        debug!("Listing available prompts");

        Ok(serde_json::json!({
            "prompts": []
        }))
    }

    async fn handle_prompts_get(&self, params: Option<Value>) -> Result<Value> {
        let params = params.ok_or_else(|| anyhow::anyhow!("Missing parameters for prompts/get"))?;

        let prompt_name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing prompt name"))?;

        debug!("Getting prompt: {}", prompt_name);

        Ok(serde_json::json!({
            "description": format!("Prompt: {}", prompt_name),
            "messages": []
        }))
    }
}

pub fn create_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        tools: Some(ToolsCapability {
            list_changed: Some(true),
        }),
        prompts: Some(PromptsCapability {
            list_changed: Some(false),
        }),
        logging: Some(LoggingCapability {}),
    }
}

// ---- tools ----

/// The content of the reply, and whether it is a refusal.
///
/// The flag is load-bearing. The CLI calls `openDiff` directly, and on a reply it
/// reads as successful it takes `content[1].text` as the new file contents and
/// records `saved: true` -- that a human reviewed and accepted the edit. Saying
/// "not supported" inside a reply marked successful invites exactly that reading.
fn dispatch_tool(tool_name: &str) -> (Vec<TextContent>, bool) {
    match tool_name {
        // Not advertised (the companion cannot see other servers' diagnostics),
        // but a direct call still gets a well-formed reply rather than -32601.
        "getDiagnostics" => (text(serde_json::json!({"diagnostics": []})), false),
        // The three the CLI really invokes -- openDiff, close_tab,
        // closeAllDiffTabs -- need an editor surface Zed exposes to extensions
        // in no form.
        _ => (
            text(Value::String(format!(
                "NOT_SUPPORTED: Tool '{tool_name}' is not available in Zed integration. \
                 File operations should be performed directly."
            ))),
            true,
        ),
    }
}

fn text(v: Value) -> Vec<TextContent> {
    let text = match v {
        Value::String(s) => s,
        other => other.to_string(),
    };
    vec![TextContent {
        type_: "text".to_string(),
        text,
    }]
}
