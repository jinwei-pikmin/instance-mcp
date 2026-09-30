//! Local tools. Each returns the MCP tool-result shape via `tool_result`.

pub mod bash;
pub mod input;
pub mod screen;
pub mod sysinfo;

pub(crate) use bash::tool_bash;
pub(crate) use input::{tool_key, tool_mouse};
pub(crate) use screen::tool_screenshot;
pub(crate) use sysinfo::tool_sys_info;

use serde_json::{json, Value};

// Wrap a structured value into the MCP tool result shape.
pub fn tool_result(structured: Value) -> Value {
    let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| structured.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": structured
    })
}
