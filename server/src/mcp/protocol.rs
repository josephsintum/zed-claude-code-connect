//! MCP over JSON-RPC 2.0: the envelope types and the request dispatcher, plus
//! the handful of tools this companion answers.
//!
//! The CLI filters the IDE's tool list before the model sees it, keeping only
//! `executeCode` and `getDiagnostics`. This server advertises neither -- there is
//! no Jupyter kernel, and Zed does not forward other language servers'
//! diagnostics here -- so the model sees no IDE tools from us at all. The tools
//! below are what the CLI itself may call.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

use super::wire::selection_tool_payload;
use crate::selection::EventBus;

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
pub struct Dispatcher {
    pub(crate) capabilities: ServerCapabilities,
    pub(crate) bus: EventBus,
    pub(crate) worktree: PathBuf,
}

impl Dispatcher {
    pub fn new(worktree: PathBuf, bus: EventBus) -> Self {
        Self {
            capabilities: create_capabilities(),
            bus,
            worktree,
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

    async fn handle_tools_list(&self) -> Result<Value> {
        debug!("Listing available tools");

        // Only list tools that are actually implemented and working
        let tools: Vec<Tool> = vec![
            Tool {
                name: "getCurrentSelection".to_string(),
                description: Some(
                    "Get the current text selection in the active editor".to_string(),
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "getLatestSelection".to_string(),
                description: Some("Get the most recent text selection".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "getWorkspaceFolders".to_string(),
                description: Some("Get the workspace folders open in the IDE".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        ];

        Ok(serde_json::json!({
            "tools": tools
        }))
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

        let content = dispatch_tool(tool_name, &self.bus, &self.worktree);

        Ok(serde_json::json!({
            "content": content,
            "isError": false
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

fn dispatch_tool(tool_name: &str, bus: &EventBus, worktree: &Path) -> Vec<TextContent> {
    match tool_name {
        "getWorkspaceFolders" => get_workspace_folders(worktree),
        "getCurrentSelection" => selection_tool(bus, "No active editor found"),
        "getLatestSelection" => selection_tool(bus, "No selection available"),
        // Not advertised (the companion cannot see other servers' diagnostics),
        // but a direct call still gets a well-formed reply rather than -32601.
        "getDiagnostics" => text(serde_json::json!({"diagnostics": []})),
        // Everything else the CLI might try -- openDiff, openFile, saveDocument,
        // close_tab and friends -- needs an editor surface Zed exposes to
        // extensions in no form. Say so, rather than fake a result: a faked
        // FILE_SAVED from openDiff would auto-approve every edit unreviewed.
        _ => text(Value::String(format!(
            "NOT_SUPPORTED: Tool '{tool_name}' is not available in Zed integration. \
             File operations should be performed directly."
        ))),
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

/// Both selection tools answer from the bus's latest selection. They differ only
/// in the message for "nothing yet"; the VS Code extension distinguishes a lost
/// editor focus, which this companion cannot observe.
fn selection_tool(bus: &EventBus, missing: &str) -> Vec<TextContent> {
    let latest = bus.latest();
    text(selection_tool_payload(latest.as_deref(), missing))
}

fn get_workspace_folders(worktree: &Path) -> Vec<TextContent> {
    let path = worktree.to_string_lossy().to_string();
    text(serde_json::json!({
        "success": true,
        "folders": [{
            "name": worktree
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace"),
            // A real file URI, percent-encoded. format!("file://{}") left a
            // space or non-ASCII byte raw, which is not a URI at all.
            "uri": lsp_types::Url::from_file_path(worktree)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| format!("file://{}", path)),
            "path": path
        }],
        "rootPath": path
    }))
}
