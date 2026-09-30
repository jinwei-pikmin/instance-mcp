//! `screenshot`: the display as MCP `image` content.

use serde_json::{json, Value};

use crate::platform;

pub fn tool_screenshot(arguments: &Value) -> Result<Value, (i64, String)> {
    let scale = arguments
        .get("scale")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let format = arguments
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("png");
    let quality = arguments
        .get("quality")
        .and_then(|v| v.as_i64())
        .unwrap_or(80);
    if !(0.05..=2.0).contains(&scale) {
        return Err((-32602, "scale must be in 0.05..=2.0".to_string()));
    }
    if format != "jpeg" && format != "png" {
        return Err((-32602, "format must be jpeg or png".to_string()));
    }
    if !(1..=100).contains(&quality) {
        return Err((-32602, "quality must be in 1..=100".to_string()));
    }

    let cap = platform::desktop()
        .capture(scale, format, quality)
        .map_err(|e| (-32000, e))?;
    let mime = if cap.format == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &cap.bytes);
    Ok(json!({
        "content": [ { "type": "image", "mimeType": mime, "data": data } ],
        "structuredContent": { "bytes": cap.bytes.len(), "scale": scale, "format": cap.format }
    }))
}
