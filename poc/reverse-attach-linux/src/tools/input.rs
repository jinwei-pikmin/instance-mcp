//! `mouse` and `key`: argument parsing on top of the `Desktop` trait.

use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::tool_result;
use crate::platform;
use crate::platform::desktop::{Button, Combo};

fn num(args: &Value, k: &str) -> Option<f64> {
    args.get(k).and_then(|v| v.as_f64())
}

pub fn tool_mouse(arguments: &Value) -> Result<Value, (i64, String)> {
    let d = platform::desktop();
    let action = arguments
        .get("action")
        .and_then(|a| a.as_str())
        .ok_or_else(|| (-32602, "missing action".to_string()))?;
    let xy = |a: &Value| -> Result<(f64, f64), (i64, String)> {
        match (num(a, "x"), num(a, "y")) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err((-32602, format!("{action} needs x and y"))),
        }
    };
    let io = |r: Result<(), String>| r.map_err(|e| (-32000, e));
    match action {
        "move" => {
            let (x, y) = xy(arguments)?;
            io(d.pointer_goto(x, y))?;
        }
        "click" | "double_click" | "right_click" => {
            if let (Some(x), Some(y)) = (num(arguments, "x"), num(arguments, "y")) {
                io(d.pointer_goto(x, y))?;
                thread::sleep(Duration::from_millis(40));
            }
            let button = if action == "right_click" {
                Button::Right
            } else {
                Button::Left
            };
            io(d.pointer_click(button))?;
            if action == "double_click" {
                thread::sleep(Duration::from_millis(60));
                io(d.pointer_click(button))?;
            }
        }
        "drag" => {
            let (x, y) = xy(arguments)?;
            let (tx, ty) = match (num(arguments, "to_x"), num(arguments, "to_y")) {
                (Some(a), Some(b)) => (a, b),
                _ => return Err((-32602, "drag needs to_x and to_y".to_string())),
            };
            io(d.pointer_goto(x, y))?;
            io(d.pointer_press(Button::Left))?;
            thread::sleep(Duration::from_millis(60));
            io(d.pointer_move_rel(tx - x, ty - y))?;
            thread::sleep(Duration::from_millis(60));
            io(d.pointer_release(Button::Left))?;
        }
        "scroll" => {
            if let (Some(x), Some(y)) = (num(arguments, "x"), num(arguments, "y")) {
                io(d.pointer_goto(x, y))?;
            }
            let dy = num(arguments, "dy").unwrap_or(0.0);
            let dx = num(arguments, "dx").unwrap_or(0.0);
            io(d.pointer_scroll(dx, dy))?;
        }
        other => return Err((-32602, format!("unknown mouse action: {other}"))),
    }
    Ok(tool_result(json!({ "ok": true, "action": action })))
}

/// `ctrl+shift+t` → modifiers `[ctrl, shift]`, key `t`. Accepts a few aliases.
pub fn parse_combo(combo: &str) -> Result<Combo, String> {
    let parts: Vec<&str> = combo
        .split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    let is_mod = |p: &str| {
        matches!(
            p.to_ascii_lowercase().as_str(),
            "ctrl" | "control" | "shift" | "alt" | "super" | "cmd" | "meta" | "win" | "altgr"
        )
    };
    let (mods, keys): (Vec<&str>, Vec<&str>) = parts.iter().partition(|p| is_mod(p));
    if keys.len() != 1 {
        return Err(format!(
            "combo must have exactly one non-modifier key: {combo}"
        ));
    }
    let norm = |m: &str| match m.to_ascii_lowercase().as_str() {
        "control" => "ctrl".to_string(),
        "cmd" | "meta" | "win" | "super" => "logo".to_string(),
        x => x.to_string(),
    };
    let key = match keys[0] {
        "Enter" | "enter" | "return" => "Return".to_string(),
        "esc" | "Esc" => "Escape".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "space".to_string(),
        k => k.to_string(),
    };
    Ok(Combo {
        modifiers: mods.iter().map(|m| norm(m)).collect(),
        key,
    })
}

pub fn tool_key(arguments: &Value) -> Result<Value, (i64, String)> {
    let d = platform::desktop();
    let action = arguments
        .get("action")
        .and_then(|a| a.as_str())
        .ok_or_else(|| (-32602, "missing action".to_string()))?;
    match action {
        "type" => {
            let text = arguments
                .get("text")
                .and_then(|t| t.as_str())
                .ok_or_else(|| (-32602, "type needs text".to_string()))?;
            d.key_type(text).map_err(|e| (-32000, e))?;
            Ok(tool_result(
                json!({ "ok": true, "action": "type", "chars": text.chars().count() }),
            ))
        }
        "press" => {
            let combo = arguments
                .get("combo")
                .and_then(|c| c.as_str())
                .ok_or_else(|| (-32602, "press needs combo".to_string()))?;
            let parsed = parse_combo(combo).map_err(|e| (-32602, e))?;
            d.key_press(&parsed).map_err(|e| (-32000, e))?;
            Ok(tool_result(
                json!({ "ok": true, "action": "press", "combo": combo }),
            ))
        }
        other => Err((-32602, format!("unknown key action: {other}"))),
    }
}
