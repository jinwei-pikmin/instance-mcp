//! Cheap orientation call for a model that just connected: what machine is this, what can
//! I do here. No side effects. Port of `SysInfoTool.swift`; facts come from the backend.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::mcp::{Tool, ToolFuture, ToolResult};
use crate::platform;

pub struct SysInfoTool {
    pub agent_version: &'static str,
    /// The tools of the server this instance is served from — bound by `McpServer::new`,
    /// so a sandbox-scoped server reports its own, narrower list.
    pub tool_names: Vec<&'static str>,
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
    fn bind_tool_names(&self, tool_names: &[&'static str]) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(SysInfoTool {
            agent_version: self.agent_version,
            tool_names: tool_names.to_vec(),
        }))
    }
    fn call<'a>(&'a self, _args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let (structured, lines) =
                platform::backend().describe(self.agent_version, &self.tool_names);
            Ok(ToolResult::text(lines.join("\n"), Some(structured)))
        })
    }
}
