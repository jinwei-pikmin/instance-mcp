//! MCP JSON-RPC dispatch (`initialize`, `tools/list`, `tools/call`), the local tool table, and re-serving an upstream MCP (e.g. @playwright/mcp) filtered by profile.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::http::{http_post, parse_mcp_body, HttpReply};
use crate::tools::{tool_bash, tool_key, tool_mouse, tool_result, tool_screenshot, tool_sys_info};

// Upstream MCP (e.g. @playwright/mcp on loopback), re-served under our tools/list.
// Mirrors Swift `UpstreamMCP`: Streamable HTTP request/response, session id held
// here and re-established on 400/404, tools cached 30 s, down → tools absent.
// ---------------------------------------------------------------------------

pub(crate) struct Upstream {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) session_id: Mutex<Option<String>>,
    pub(crate) cache: Mutex<Option<(std::time::Instant, Vec<Value>)>>,
}

impl Upstream {
    /// MCP_UPSTREAM="browser=http://127.0.0.1:8794/mcp[,name=url...]"
    pub(crate) fn from_env() -> Vec<Arc<Upstream>> {
        std::env::var("MCP_UPSTREAM")
            .unwrap_or_default()
            .split(',')
            .filter_map(|e| e.trim().split_once('='))
            .map(|(n, u)| {
                Arc::new(Upstream {
                    name: n.trim().to_string(),
                    url: u.trim().to_string(),
                    session_id: Mutex::new(None),
                    cache: Mutex::new(None),
                })
            })
            .collect()
    }

    fn post(&self, msg: &Value) -> Result<HttpReply, String> {
        let sid = self.session_id.lock().ok().and_then(|g| g.clone());
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        if let Some(s) = sid.as_deref() {
            headers.push(("Mcp-Session-Id", s));
        }
        http_post(
            &self.url,
            &headers,
            &msg.to_string(),
            Duration::from_secs(90),
        )
    }

    fn initialize(&self) -> Result<(), String> {
        // Never send a stale id on initialize: the upstream answers 404 to it.
        if let Ok(mut g) = self.session_id.lock() {
            *g = None;
        }
        let r = self.post(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "instance-mcp-rpi", "version": "0.4.0"}}
        }))?;
        if !(200..300).contains(&r.status) {
            return Err(format!(
                "upstream {} initialize HTTP {}",
                self.name, r.status
            ));
        }
        let sid = r
            .headers
            .iter()
            .find(|(k, _)| k == "mcp-session-id")
            .map(|(_, v)| v.clone());
        if let Ok(mut g) = self.session_id.lock() {
            *g = sid;
        }
        let _ = self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        if let Ok(mut c) = self.cache.lock() {
            *c = None;
        }
        Ok(())
    }

    pub(crate) fn rpc(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        if self
            .session_id
            .lock()
            .ok()
            .map(|g| g.is_none())
            .unwrap_or(true)
        {
            self.initialize()?;
        }
        let mut msg = json!({"jsonrpc": "2.0", "id": 1, "method": method});
        if let Some(p) = &params {
            msg["params"] = p.clone();
        }
        let mut r = self.post(&msg)?;
        if r.status == 404 || r.status == 400 {
            // Upstream lost our session (restart). One re-init, one retry.
            self.initialize()?;
            r = self.post(&msg)?;
        }
        if !(200..300).contains(&r.status) {
            return Err(format!(
                "upstream {} HTTP {}: {}",
                self.name,
                r.status,
                &r.body[..r.body.len().min(120)]
            ));
        }
        let v = parse_mcp_body(&r.body)?;
        if let Some(e) = v.get("error") {
            return Err(format!("upstream {} error: {e}", self.name));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub(crate) fn tools(&self) -> Vec<Value> {
        if let Ok(c) = self.cache.lock() {
            if let Some((at, t)) = c.as_ref() {
                if at.elapsed() < Duration::from_secs(30) {
                    return t.clone();
                }
            }
        }
        let list = match self.rpc("tools/list", None) {
            Ok(r) => r
                .get("tools")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default(),
            Err(e) => {
                eprintln!("upstream {}: {e}", self.name);
                // Do not cache a failure: the next tools/list retries immediately.
                if let Ok(mut c) = self.cache.lock() {
                    *c = None;
                }
                return Vec::new();
            }
        };
        if let Ok(mut c) = self.cache.lock() {
            *c = Some((std::time::Instant::now(), list.clone()));
        }
        list
    }
}

/// Same list as Swift `ToolProfile.sandboxBrowserTools`: navigate / read / interact.
/// Everything else from the upstream (run_code_unsafe, upload, pdf, network, raw
/// mouse-by-coordinate, dialogs, close, and any new tool) is denied under sandbox.
pub(crate) const SANDBOX_BROWSER_TOOLS: &[&str] = &[
    "browser_click",
    "browser_console_messages",
    "browser_evaluate",
    "browser_fill_form",
    "browser_find",
    "browser_hover",
    "browser_navigate",
    "browser_navigate_back",
    "browser_press_key",
    "browser_resize",
    "browser_select_option",
    "browser_snapshot",
    "browser_tabs",
    "browser_take_screenshot",
    "browser_type",
    "browser_wait_for",
];

pub(crate) fn upstream_tool_allowed(name: &str, profile: &str) -> bool {
    profile != "sandbox" || SANDBOX_BROWSER_TOOLS.contains(&name)
}

pub(crate) static UPSTREAMS: Mutex<Vec<Arc<Upstream>>> = Mutex::new(Vec::new());

pub(crate) fn upstreams() -> Vec<Arc<Upstream>> {
    UPSTREAMS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Upstream tools visible to `profile`, excluding names that collide with local tools.
pub(crate) fn upstream_tools_for(
    profile: &str,
    local_names: &[&str],
) -> Vec<(Arc<Upstream>, Value)> {
    let mut out = Vec::new();
    for up in upstreams() {
        for t in up.tools() {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if local_names.contains(&name) || !upstream_tool_allowed(name, profile) {
                continue;
            }
            out.push((up.clone(), t));
        }
    }
    out
}

pub(crate) fn upstream_owning(name: &str, profile: &str) -> Option<Arc<Upstream>> {
    if !upstream_tool_allowed(name, profile) {
        return None;
    }
    upstreams().into_iter().find(|up| {
        up.tools()
            .iter()
            .any(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
    })
}

// ---------------------------------------------------------------------------
// answer(): MCP JSON-RPC dispatch
// ---------------------------------------------------------------------------

pub(crate) fn answer(text: &str, profile: &str) -> Option<String> {
    let req: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // Notifications have no id -> no response.
    let id = req.get("id").cloned()?;

    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "instance-mcp-rpi",
                "version": "0.3.0"
            }
        })),
        "tools/list" => Ok(json!({ "tools": tool_list(profile) })),
        "tools/call" => handle_tool_call(&params, profile),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    let response = match result {
        Ok(res) => json!({ "jsonrpc": "2.0", "id": id, "result": res }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    };

    Some(response.to_string())
}

pub(crate) const LOCAL_TOOL_NAMES: &[&str] = &["sys_info", "screenshot", "bash", "mouse", "key"];

pub(crate) fn tool_list(profile: &str) -> Value {
    let sys_info = json!({
        "name": "sys_info",
        "description": "Report OS, CPU, memory, architecture and hostname of this node.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
    });
    let screenshot = json!({
        "name": "screenshot",
        "description": "Capture the node's Wayland display (via grim) and return it as an image. \
                        Default PNG at scale 0.5 (960x540 for a 1080p output). jpeg only if the \
                        node's grim was built with JPEG support (Debian's is not).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "scale":   { "type": "number", "description": "output scale factor, default 0.5" },
                "format":  { "type": "string", "enum": ["jpeg", "png"], "description": "default png" },
                "quality": { "type": "integer", "description": "jpeg quality 1-100, default 80" }
            },
            "additionalProperties": false
        }
    });
    let bash = json!({
        "name": "bash",
        "description": "Run a command with `bash -c` on this node as the daemon user. Returns stdout, \
                        stderr, exit code and duration. The whole process group is killed on timeout \
                        (exit 137, timed_out=true). Output is capped per stream.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command":          { "type": "string" },
                "cwd":              { "type": "string", "description": "working directory; leading ~ expands to $HOME" },
                "timeout_secs":     { "type": "integer", "description": "default 60, max 600" },
                "max_output_bytes": { "type": "integer", "description": "per stream, default 65536, max 1048576" }
            },
            "required": ["command"],
            "additionalProperties": false
        }
    });

    let mouse = json!({
        "name": "mouse",
        "description": "Pointer input on this node's Wayland display (wlroots virtual pointer). Coordinates \
                        are display pixels = screenshot pixels at scale 1 (1920x1080 here). Actions: move, \
                        click, double_click, right_click, drag (x,y → to_x,to_y), scroll (dy/dx in wheel \
                        notches/lines, positive = down/right).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["move", "click", "double_click", "right_click", "drag", "scroll"] },
                "x": { "type": "number" }, "y": { "type": "number" },
                "to_x": { "type": "number" }, "to_y": { "type": "number" },
                "dx": { "type": "number" }, "dy": { "type": "number" }
            },
            "required": ["action"],
            "additionalProperties": false
        }
    });
    let key = json!({
        "name": "key",
        "description": "Keyboard input via wtype. `type`: send text (unicode, layout independent). `press`: \
                        a key combo such as \"Return\", \"Tab\", \"ctrl+c\", \"ctrl+shift+t\", \"alt+F4\" \
                        (xkb key names; modifiers ctrl/shift/alt/super).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["type", "press"] },
                "text": { "type": "string" },
                "combo": { "type": "string" }
            },
            "required": ["action"],
            "additionalProperties": false
        }
    });

    // Both profiles get everything: a lent node is only useful if the agent can act on it,
    // and the macOS sandbox profile already leaks a shell through `osascript`.
    let _ = profile;
    let mut tools = vec![sys_info, screenshot, bash, mouse, key];
    for (_, t) in upstream_tools_for(profile, LOCAL_TOOL_NAMES) {
        tools.push(t);
    }
    Value::Array(tools)
}

pub(crate) fn handle_tool_call(params: &Value, profile: &str) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| (-32602, "missing tool name".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name {
        "sys_info" => Ok(tool_result(tool_sys_info())),
        "screenshot" => tool_screenshot(&arguments),
        "bash" => {
            let _ = profile;
            tool_bash(&arguments)
        }
        "mouse" => tool_mouse(&arguments),
        "key" => tool_key(&arguments),
        other => match upstream_owning(other, profile) {
            Some(up) => up
                .rpc(
                    "tools/call",
                    Some(json!({"name": other, "arguments": arguments})),
                )
                .map_err(|e| (-32000, e)),
            None => Err((-32601, format!("unknown tool: {other}"))),
        },
    }
}

// ---------------------------------------------------------------------------
