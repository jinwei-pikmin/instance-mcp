//! Cheap orientation call for a model that just connected: what machine is this, what can
//! I do here. No side effects. Port of `SysInfoTool.swift`; facts come from the backend.

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::mcp::{Tool, ToolFuture, ToolResult};
use crate::platform;

pub struct SysInfoTool {
    pub agent_version: &'static str,
    /// Filled in once the server's tool list is known, so the report matches reality.
    pub tool_names: OnceLock<Vec<&'static str>>,
}

impl Tool for SysInfoTool {
    fn name(&self) -> &'static str {
        "sys_info"
    }
    fn description(&self) -> String {
        format!(
            "Describe this machine ({}): hostname, OS version, hardware, user, desktop session, \
             Tailscale addresses, and which tools work here. Call this first to learn what the \
             other tools can do.",
            platform::backend().os_label()
        )
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call<'a>(&'a self, _args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let names = self.tool_names.get().cloned().unwrap_or_default();
            let (structured, lines) = platform::backend().describe(self.agent_version, &names);
            Ok(ToolResult::text(lines.join("\n"), Some(structured)))
        })
    }
}
