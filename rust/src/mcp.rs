//! Transport-independent MCP server core. Port of `MCPServer.swift`, `Tool.swift`,
//! `ToolProfile.swift` and the JSON-RPC parts of `JSONRPC.swift`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Map, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Older clients (kiro-cli, some SDKs) negotiate these; we speak the same subset.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

// MARK: - JSON-RPC

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;

    pub fn invalid_params(why: impl std::fmt::Display) -> Self {
        Self {
            code: Self::INVALID_PARAMS,
            message: format!("Invalid params: {why}"),
        }
    }
    fn json(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }
}

#[derive(Debug, Clone)]
pub struct RpcRequest {
    /// None (absent or null) ⇒ notification.
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

impl RpcRequest {
    /// Parse raw bytes into a request. Batches are rejected (removed in 2025-06-18).
    pub fn parse(bytes: &[u8]) -> Result<RpcRequest, RpcError> {
        let v: Value = serde_json::from_slice(bytes).map_err(|e| RpcError {
            code: RpcError::PARSE_ERROR,
            message: format!("Parse error: {e}"),
        })?;
        Self::from_value(v)
    }

    pub fn from_value(v: Value) -> Result<RpcRequest, RpcError> {
        let invalid = |m: &str| RpcError {
            code: RpcError::INVALID_REQUEST,
            message: m.into(),
        };
        let obj = match v {
            Value::Object(o) => o,
            Value::Array(a) if !a.is_empty() => {
                return Err(invalid("batch requests are not supported"))
            }
            _ => return Err(invalid("request must be a JSON object")),
        };
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(invalid("jsonrpc must be \"2.0\""));
        }
        let method = obj
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("method is required"))?;
        let id = match obj.get("id") {
            None | Some(Value::Null) => None,
            Some(id @ (Value::String(_) | Value::Number(_))) => Some(id.clone()),
            Some(_) => return Err(invalid("id must be a string or number")),
        };
        Ok(RpcRequest {
            id,
            method: method.to_string(),
            params: obj.get("params").cloned(),
        })
    }
}

/// Whole-valued floats as integers (`1800.0` → `1800`), as the Swift build's encoder
/// writes them. Mac-first clients decode fields such as display sizes into `Int`, and
/// Foundation's JSONDecoder may refuse `1800.0` for an `Int`.
pub fn swift_numbers(v: Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15 => Value::from(f as i64),
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(swift_numbers).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (k, swift_numbers(v))).collect())
        }
        other => other,
    }
}

pub fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn rpc_error(id: Value, e: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": e.json()})
}

// MARK: - Tools

pub enum Content {
    Text(String),
    Image { data_b64: String, mime_type: String },
}

pub struct ToolResult {
    pub content: Vec<Content>,
    pub is_error: bool,
    /// Surfaced as `structuredContent` (MCP 2025-06-18).
    pub structured: Option<Value>,
}

impl ToolResult {
    pub fn text(s: impl Into<String>, structured: Option<Value>) -> Self {
        Self {
            content: vec![Content::Text(s.into())],
            is_error: false,
            structured,
        }
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(s.into())],
            is_error: true,
            structured: None,
        }
    }

    pub fn json(&self) -> Value {
        let content: Vec<Value> = self
            .content
            .iter()
            .map(|c| match c {
                Content::Text(s) => json!({"type": "text", "text": s}),
                Content::Image {
                    data_b64,
                    mime_type,
                } => {
                    json!({"type": "image", "data": data_b64, "mimeType": mime_type})
                }
            })
            .collect();
        let mut o = Map::new();
        o.insert("content".into(), Value::Array(content));
        if self.is_error {
            o.insert("isError".into(), Value::Bool(true));
        }
        if let Some(s) = &self.structured {
            o.insert("structuredContent".into(), s.clone());
        }
        Value::Object(o)
    }
}

/// How a tool call can fail. `Rpc` is a protocol error (bad arguments); `Tool` is a
/// tool-level failure, returned as an `isError` result so the model sees it.
pub enum ToolFail {
    Rpc(RpcError),
    Tool(String),
}

impl From<RpcError> for ToolFail {
    fn from(e: RpcError) -> Self {
        ToolFail::Rpc(e)
    }
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolResult, ToolFail>> + Send + 'a>>;

/// A tool the MCP server exposes. Implementations must be safe to call concurrently.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    /// JSON Schema for `arguments`.
    fn input_schema(&self) -> Value;
    fn call<'a>(&'a self, args: &'a Value) -> ToolFuture<'a>;

    fn descriptor(&self) -> Value {
        json!({"name": self.name(), "description": self.description(), "inputSchema": self.input_schema()})
    }

    /// For tools whose answer depends on the server they are served from (`sys_info`
    /// reports the tool list): a copy bound to `tool_names`, the list of the server being
    /// built. Called by `McpServer::new`, so a `scoped` server rebinds too.
    fn bind_tool_names(&self, _tool_names: &[&'static str]) -> Option<Arc<dyn Tool>> {
        None
    }
}

/// Which tools a connection may see and call. `owner` is the logged-in human's own CLI:
/// everything. `sandbox` is an agent in an `openab-pty` session this machine was lent to:
/// no `exec*` (the agent already has a shell in its sandbox) and an allowlisted
/// `browser_*` subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProfile {
    Owner,
    Sandbox,
}

impl ToolProfile {
    pub const SANDBOX_BROWSER_TOOLS: &'static [&'static str] = &[
        "browser_navigate",
        "browser_navigate_back",
        "browser_snapshot",
        "browser_find",
        "browser_click",
        "browser_type",
        "browser_fill_form",
        "browser_press_key",
        "browser_hover",
        "browser_select_option",
        "browser_wait_for",
        "browser_tabs",
        "browser_take_screenshot",
        "browser_console_messages",
        "browser_resize",
        "browser_evaluate",
    ];

    pub const ALL: [ToolProfile; 2] = [ToolProfile::Owner, ToolProfile::Sandbox];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolProfile::Owner => "owner",
            ToolProfile::Sandbox => "sandbox",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub fn allows(self, tool: &str) -> bool {
        match self {
            ToolProfile::Owner => true,
            ToolProfile::Sandbox => {
                if tool.starts_with("exec") {
                    return false;
                }
                if tool.starts_with("browser_") {
                    return Self::SANDBOX_BROWSER_TOOLS.contains(&tool);
                }
                true
            }
        }
    }
}

// MARK: - Server

#[derive(Clone)]
pub struct McpServer {
    pub name: String,
    pub version: String,
    pub instructions: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    by_name: HashMap<&'static str, Arc<dyn Tool>>,
}

impl McpServer {
    pub fn new(
        name: &str,
        version: &str,
        instructions: Option<String>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let names: Vec<&'static str> = tools.iter().map(|t| t.name()).collect();
        let tools: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| t.bind_tool_names(&names).unwrap_or(t))
            .collect();
        let by_name = tools.iter().map(|t| (t.name(), t.clone())).collect();
        Self {
            name: name.into(),
            version: version.into(),
            instructions,
            tools,
            by_name,
        }
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// The same server with its tool list narrowed to `profile` and, optionally, its own
    /// instructions. `tools/call` on an omitted tool is an *unknown tool* error,
    /// indistinguishable from one that never existed — the agent is not told there is
    /// something it may not have.
    pub fn scoped(&self, profile: ToolProfile, instructions: Option<String>) -> Self {
        let tools = self
            .tools
            .iter()
            .filter(|t| profile.allows(t.name()))
            .cloned()
            .collect();
        let instructions = instructions.or_else(|| self.instructions.clone());
        Self::new(&self.name, &self.version, instructions, tools)
    }

    /// Returns None for notifications (no response body) — the HTTP layer answers 202.
    pub async fn handle(&self, req: &RpcRequest) -> Option<Value> {
        let id = req.id.clone()?;
        Some(match self.dispatch(req).await {
            Ok(result) => rpc_result(id, swift_numbers(result)),
            Err(e) => rpc_error(id, &e),
        })
    }

    async fn dispatch(&self, req: &RpcRequest) -> Result<Value, RpcError> {
        let param = |k: &str| req.params.as_ref().and_then(|p| p.get(k));
        match req.method.as_str() {
            "initialize" => {
                let requested = param("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION);
                let negotiated = if SUPPORTED_VERSIONS.contains(&requested) {
                    requested
                } else {
                    PROTOCOL_VERSION
                };
                let mut r = json!({
                    "protocolVersion": negotiated,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": self.name, "version": self.version},
                });
                if let Some(i) = &self.instructions {
                    r["instructions"] = Value::String(i.clone());
                }
                Ok(r)
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                Ok(json!({"tools": self.tools.iter().map(|t| t.descriptor()).collect::<Vec<_>>()}))
            }
            "tools/call" => {
                let name = param("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::invalid_params("missing tool name"))?;
                let tool = self
                    .by_name
                    .get(name)
                    .ok_or_else(|| RpcError::invalid_params(format!("unknown tool: {name}")))?;
                let empty = json!({});
                let args = param("arguments").unwrap_or(&empty);
                match tool.call(args).await {
                    Ok(r) => Ok(r.json()),
                    Err(ToolFail::Rpc(e)) => Err(e),
                    // Tool-level failures are results, not protocol errors — the model should see them.
                    Err(ToolFail::Tool(msg)) => Ok(ToolResult::error(msg).json()),
                }
            }
            "resources/list" | "resources/templates/list" => Ok(json!({"resources": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            m => Err(RpcError {
                code: RpcError::METHOD_NOT_FOUND,
                message: format!("Method not found: {m}"),
            }),
        }
    }
}

// MARK: - argument helpers shared by tools

pub fn arg_str<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str)
}

pub fn arg_f64(args: &Value, k: &str) -> Option<f64> {
    args.get(k).and_then(Value::as_f64)
}

/// Integer-valued numbers only (matches Swift `JSONValue.intValue`).
pub fn arg_i64(args: &Value, k: &str) -> Option<i64> {
    let n = args.get(k)?;
    n.as_i64().or_else(|| {
        n.as_f64()
            .filter(|f| f.fract() == 0.0 && f.abs() < i64::MAX as f64)
            .map(|f| f as i64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo(&'static str);
    impl Tool for Echo {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> String {
            "echo".into()
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn call<'a>(&'a self, args: &'a Value) -> ToolFuture<'a> {
            Box::pin(async move {
                match arg_str(args, "fail") {
                    Some(m) => Err(ToolFail::Tool(m.into())),
                    None => Ok(ToolResult::text("hi", None)),
                }
            })
        }
    }

    fn server() -> McpServer {
        McpServer::new(
            "t",
            "0",
            Some("be nice".into()),
            vec![Arc::new(Echo("exec")), Arc::new(Echo("sys_info"))],
        )
    }

    async fn call(s: &McpServer, body: Value) -> Option<Value> {
        s.handle(&RpcRequest::from_value(body).unwrap()).await
    }

    #[tokio::test]
    async fn initialize_negotiates_version() {
        let s = server();
        let r = call(&s, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}))
            .await
            .unwrap();
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["instructions"], "be nice");
        let r = call(&s, json!({"jsonrpc":"2.0","id":"x","method":"initialize","params":{"protocolVersion":"1999"}}))
            .await
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        assert!(call(
            &server(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn tool_errors_are_results_unknown_tools_are_rpc_errors() {
        let s = server();
        let r = call(&s, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"exec","arguments":{"fail":"boom"}}}))
            .await
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "boom");
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope"}}),
        )
        .await
        .unwrap();
        assert_eq!(r["error"]["code"], RpcError::INVALID_PARAMS);
        let r = call(&s, json!({"jsonrpc":"2.0","id":4,"method":"bogus"}))
            .await
            .unwrap();
        assert_eq!(r["error"]["code"], RpcError::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn sandbox_scope_hides_exec() {
        let s = server().scoped(ToolProfile::Sandbox, None);
        assert_eq!(s.tool_names(), vec!["sys_info"]);
        let r = call(
            &s,
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"exec"}}),
        )
        .await
        .unwrap();
        assert_eq!(r["error"]["message"], "Invalid params: unknown tool: exec");
        assert!(ToolProfile::Sandbox.allows("browser_click"));
        assert!(!ToolProfile::Sandbox.allows("browser_run_code_unsafe"));
    }

    #[test]
    fn whole_floats_encode_as_integers() {
        let v = swift_numbers(json!({"w": 1728.0, "s": 1.6666, "n": [0.0, -3.0], "i": 7}));
        assert_eq!(v.to_string(), r#"{"i":7,"n":[0,-3],"s":1.6666,"w":1728}"#);
    }

    #[test]
    fn parse_rejects_batches_and_bad_versions() {
        assert_eq!(
            RpcRequest::parse(b"[{}]").unwrap_err().code,
            RpcError::INVALID_REQUEST
        );
        assert_eq!(
            RpcRequest::parse(b"{nope").unwrap_err().code,
            RpcError::PARSE_ERROR
        );
        assert_eq!(
            RpcRequest::parse(br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#)
                .unwrap_err()
                .code,
            RpcError::INVALID_REQUEST
        );
    }
}
