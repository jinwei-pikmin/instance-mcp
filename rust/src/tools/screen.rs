//! `screenshot`, `mouse`, `key`: the see→act→see loop, written against `Desktop` so every
//! backend shares one argument schema. Ports of `ScreenshotTool.swift`, `MouseTool.swift`,
//! `KeyTool.swift` and `Input.swift` with the same coordinate contract: display POINTS, and
//! at `scale` 1.0 one screenshot pixel is one point.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder};
use serde_json::{json, Value};

use crate::mcp::{
    arg_f64, arg_i64, arg_str, Content, RpcError, Tool, ToolFail, ToolFuture, ToolResult,
};
use crate::platform::desktop::{Button, Desktop, Key, NamedKey};

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn fail(e: String) -> ToolFail {
    ToolFail::Tool(e)
}

// MARK: - screenshot

pub struct ScreenshotTool(pub Arc<dyn Desktop>);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn intersect(self, o: Rect) -> Option<Rect> {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = (
            (self.x + self.w).min(o.x + o.w),
            (self.y + self.h).min(o.y + o.h),
        );
        (x1 > x0 && y1 > y0).then_some(Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}

impl Tool for ScreenshotTool {
    fn name(&self) -> &'static str {
        "screenshot"
    }
    fn description(&self) -> String {
        "Capture the current screen of this machine and return it as an image. Use `display` \
         (0-based index, default 0 = the display at the layout origin) on multi-monitor setups. \
         `scale` (default 1.0 = one pixel per display point) resamples. `region` {x,y,width,height} \
         in display points crops before scaling — use it to zoom into a dialog or a panel cheaply. \
         `format` jpeg (default, `quality` 0–1) or png. All coordinates are display POINTS, the same \
         space `mouse` uses; a crop's pixel (px,py) maps to point (region.x + px/scale, region.y + py/scale). \
         The first call may wait for a human to approve screen sharing on this machine's screen."
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "display": {"type": "integer", "description": "Display index, 0 = the display at the layout origin.", "default": 0},
                "scale": {"type": "number", "description": "Output pixels per display point. Default 1.0.", "minimum": 0.1, "maximum": 2.0},
                "region": {"type": "object", "description": "Crop rectangle in display points.", "properties": {
                    "x": {"type": "number"}, "y": {"type": "number"}, "width": {"type": "number"}, "height": {"type": "number"}},
                    "required": ["x", "y", "width", "height"]},
                "format": {"type": "string", "enum": ["jpeg", "png"], "default": "jpeg"},
                "quality": {"type": "number", "description": "JPEG quality 0–1. Default 0.7.", "minimum": 0.1, "maximum": 1.0},
            },
        })
    }
    fn call<'a>(&'a self, a: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let index = arg_i64(a, "display").unwrap_or(0);
            let scale = arg_f64(a, "scale").unwrap_or(1.0).clamp(0.1, 2.0);
            let png = arg_str(a, "format") == Some("png");
            let quality = arg_f64(a, "quality").unwrap_or(0.7).clamp(0.1, 1.0);
            let region = match a.get("region") {
                None | Some(Value::Null) => None,
                Some(r) => {
                    let f = |k| r.get(k).and_then(Value::as_f64);
                    match (f("x"), f("y"), f("width"), f("height")) {
                        (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
                            Some(Rect { x, y, w, h })
                        }
                        _ => {
                            return Err(RpcError::invalid_params(
                                "region needs x, y, width>0, height>0",
                            )
                            .into())
                        }
                    }
                }
            };

            let cap = self.0.capture().await.map_err(fail)?;
            let d = usize::try_from(index)
                .ok()
                .and_then(|i| cap.displays.get(i))
                .ok_or_else(|| {
                    fail(format!(
                        "display {index} out of range; {} display(s) available",
                        cap.displays.len()
                    ))
                })?;
            let whole = Rect {
                x: 0.0,
                y: 0.0,
                w: d.width,
                h: d.height,
            };
            let r = match region {
                Some(r) => r
                    .intersect(whole)
                    .ok_or_else(|| fail(format!("region lies outside display {index}")))?,
                None => whole,
            };

            // Points → source pixels (the capture spans the whole layout).
            let ppp = cap.pixels_per_point;
            let (iw, ih) = (cap.image.width(), cap.image.height());
            let px = ((d.x + r.x) * ppp).round().clamp(0.0, iw as f64 - 1.0) as u32;
            let py = ((d.y + r.y) * ppp).round().clamp(0.0, ih as f64 - 1.0) as u32;
            let pw = ((r.w * ppp).round() as u32).clamp(1, iw - px);
            let ph = ((r.h * ppp).round() as u32).clamp(1, ih - py);
            let cropped = image::imageops::crop_imm(&cap.image, px, py, pw, ph).to_image();
            let (ow, oh) = (
                ((r.w * scale).round() as u32).max(1),
                ((r.h * scale).round() as u32).max(1),
            );
            let out = if (ow, oh) == (pw, ph) {
                cropped
            } else {
                image::imageops::resize(&cropped, ow, oh, FilterType::Triangle)
            };

            let mut buf = Vec::new();
            let mime = if png {
                image::codecs::png::PngEncoder::new(Cursor::new(&mut buf))
                    .write_image(out.as_raw(), ow, oh, image::ExtendedColorType::Rgba8)
                    .map_err(|e| fail(format!("png encode: {e}")))?;
                "image/png"
            } else {
                let rgb = DynamicImage::ImageRgba8(out).to_rgb8();
                image::codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut buf,
                    (quality * 100.0).round() as u8,
                )
                .write_image(rgb.as_raw(), ow, oh, image::ExtendedColorType::Rgb8)
                .map_err(|e| fail(format!("jpeg encode: {e}")))?;
                "image/jpeg"
            };

            let crop = if region.is_some() {
                format!(
                    " region {},{} {}×{}pt",
                    r.x as i64, r.y as i64, r.w as i64, r.h as i64
                )
            } else {
                String::new()
            };
            let caption = format!(
                "display {index}: {}×{} pt{crop} → {ow}×{oh} px {mime} ({} KiB), scale {scale}",
                d.width as i64,
                d.height as i64,
                buf.len() / 1024
            );
            let structured = json!({
                "display": index,
                "points": {"width": d.width, "height": d.height},
                "region": {"x": r.x, "y": r.y, "width": r.w, "height": r.h},
                "image": {"width": ow, "height": oh, "bytes": buf.len()},
                "scale": scale,
                "pixels_per_point": ppp,
            });
            Ok(ToolResult {
                content: vec![
                    Content::Image {
                        data_b64: base64::engine::general_purpose::STANDARD.encode(&buf),
                        mime_type: mime.into(),
                    },
                    Content::Text(caption),
                ],
                is_error: false,
                structured: Some(structured),
            })
        })
    }
}

// MARK: - keys

/// Modifier names. `cmd` means Ctrl here: Linux apps put their shortcuts on Ctrl, so a
/// model trained on macOS combos (`cmd+c`) does the right thing.
fn modifier(name: &str) -> Option<NamedKey> {
    Some(match name {
        "ctrl" | "control" | "cmd" | "command" => NamedKey::Ctrl,
        "alt" | "opt" | "option" => NamedKey::Alt,
        "shift" => NamedKey::Shift,
        "super" | "meta" | "win" | "windows" => NamedKey::Super,
        _ => return None,
    })
}

const KEY_NAMES: &[(&str, NamedKey)] = &[
    ("return", NamedKey::Return),
    ("enter", NamedKey::Return),
    ("tab", NamedKey::Tab),
    ("space", NamedKey::Space),
    ("delete", NamedKey::Backspace), // macOS naming: "delete" is backspace
    ("backspace", NamedKey::Backspace),
    ("forwarddelete", NamedKey::Delete),
    ("escape", NamedKey::Escape),
    ("esc", NamedKey::Escape),
    ("insert", NamedKey::Insert),
    ("home", NamedKey::Home),
    ("end", NamedKey::End),
    ("pageup", NamedKey::PageUp),
    ("pagedown", NamedKey::PageDown),
    ("left", NamedKey::Left),
    ("right", NamedKey::Right),
    ("up", NamedKey::Up),
    ("down", NamedKey::Down),
    ("capslock", NamedKey::CapsLock),
    ("print", NamedKey::PrintScreen),
    ("printscreen", NamedKey::PrintScreen),
    ("menu", NamedKey::Menu),
    ("volumeup", NamedKey::VolumeUp),
    ("volumedown", NamedKey::VolumeDown),
    ("mute", NamedKey::Mute),
];

#[derive(Debug, PartialEq)]
struct Combo {
    mods: Vec<NamedKey>,
    key: Key,
}

/// `"ctrl+shift+t"`, `"return"`, `"f5"`. Case-insensitive; the last token is the key.
fn parse_combo(s: &str) -> Result<Combo, String> {
    let parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).collect();
    let (key_name, mod_names) = parts.split_last().ok_or("empty key")?;
    if key_name.is_empty() {
        return Err(format!("empty key in '{s}' (spell + as \"plus\")"));
    }
    let mods = mod_names
        .iter()
        .map(|m| modifier(m).ok_or_else(|| format!("unknown modifier '{m}' in '{s}'")))
        .collect::<Result<Vec<_>, _>>()?;
    let key = if key_name == "plus" {
        Key::Char('+')
    } else if let Some((_, k)) = KEY_NAMES.iter().find(|(n, _)| n == key_name) {
        Key::Named(*k)
    } else if let Some(n) = key_name
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| (1..=24).contains(n))
    {
        Key::Named(NamedKey::F(n))
    } else {
        let mut chars = key_name.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Key::Char(c),
            _ => {
                let names: Vec<&str> = KEY_NAMES.iter().map(|(n, _)| *n).collect();
                return Err(format!(
                    "unknown key '{key_name}' in '{s}'; use a letter, digit, punctuation, f1–f24, or one of: {}",
                    names.join(" ")
                ));
            }
        }
    };
    Ok(Combo { mods, key })
}

async fn tap(d: &dyn Desktop, c: &Combo) -> Result<(), String> {
    for m in &c.mods {
        d.key(Key::Named(*m), true).await?;
    }
    let r = async {
        d.key(c.key, true).await?;
        sleep_ms(15).await;
        d.key(c.key, false).await
    }
    .await;
    // Always release modifiers, even if the key failed, so none stays stuck down.
    for m in c.mods.iter().rev() {
        let _ = d.key(Key::Named(*m), false).await;
    }
    r
}

pub struct KeyTool(pub Arc<dyn Desktop>);

impl Tool for KeyTool {
    fn name(&self) -> &'static str {
        "key"
    }
    fn description(&self) -> String {
        "Keyboard input on this machine. `type`: send `text` into the focused app character by \
         character (newlines become Return; on Linux, non-ASCII characters such as CJK go through the \
         desktop's Unicode entry, ctrl+shift+u, which GTK/Qt apps and browsers accept). `press`: one or more key combos in `keys`, e.g. \
         [\"ctrl+l\", \"ctrl+a\"], [\"return\"], [\"alt+f4\"]; modifiers ctrl/alt/shift/super (`cmd` is \
         accepted and means ctrl, since Linux apps use ctrl for shortcuts); keys are letters, digits, \
         punctuation, or return tab space backspace delete escape left right up down home end pageup \
         pagedown insert f1–f24."
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["type", "press"]},
                "text": {"type": "string", "description": "for type"},
                "keys": {"type": "array", "items": {"type": "string"}, "description": "for press; combos executed in order"},
                "delay_ms": {"type": "integer", "description": "pause between keystrokes/combos. Default 10 (type) / 50 (press)."},
            },
            "required": ["action"],
        })
    }
    fn call<'a>(&'a self, a: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let d = self.0.as_ref();
            match arg_str(a, "action") {
                Some("type") => {
                    let text = arg_str(a, "text")
                        .filter(|t| !t.is_empty())
                        .ok_or_else(|| RpcError::invalid_params("text is required"))?;
                    if text.chars().count() > 20_000 {
                        return Err(fail("text too long (max 20000 characters)".into()));
                    }
                    let delay = arg_i64(a, "delay_ms").unwrap_or(10).max(0) as u64;
                    let mut n = 0;
                    for c in text.chars().filter(|c| *c != '\r') {
                        if c == '\n' {
                            let k = Key::Named(NamedKey::Return);
                            d.key(k, true).await.map_err(fail)?;
                            d.key(k, false).await.map_err(fail)?;
                        } else {
                            d.type_char(c).await.map_err(fail)?;
                        }
                        n += 1;
                        if delay > 0 {
                            sleep_ms(delay).await;
                        }
                    }
                    Ok(ToolResult::text(format!("typed {n} character(s)"), None))
                }
                Some("press") => {
                    let keys = a
                        .get("keys")
                        .and_then(Value::as_array)
                        .filter(|k| !k.is_empty());
                    let keys = keys.ok_or_else(|| RpcError::invalid_params("keys is required"))?;
                    let delay = arg_i64(a, "delay_ms").unwrap_or(50).max(0) as u64;
                    let mut done = vec![];
                    for k in keys {
                        let s = k
                            .as_str()
                            .ok_or_else(|| RpcError::invalid_params("keys must be strings"))?;
                        tap(d, &parse_combo(s).map_err(fail)?).await.map_err(fail)?;
                        done.push(s);
                        sleep_ms(delay).await;
                    }
                    Ok(ToolResult::text(
                        format!("pressed {}", done.join(", ")),
                        None,
                    ))
                }
                Some(other) => {
                    Err(RpcError::invalid_params(format!("unknown action {other}")).into())
                }
                None => Err(RpcError::invalid_params("action is required").into()),
            }
        })
    }
}

// MARK: - mouse

pub struct MouseTool(pub Arc<dyn Desktop>);

impl Tool for MouseTool {
    fn name(&self) -> &'static str {
        "mouse"
    }
    fn description(&self) -> String {
        "Mouse input on this machine. Coordinates are display POINTS relative to the top-left of \
         `display` — the same space as `screenshot` (at the default scale 1.0, image pixel == point; \
         with a `region` crop add the region origin; at other scales divide pixels by scale). Actions: \
         `move`, `click` (left), `double_click`, `right_click`, `drag` (from x,y to to_x,to_y), \
         `scroll` (dx/dy in wheel clicks; positive dy scrolls content up — i.e. wheel toward you is \
         negative). `modifiers` (ctrl/alt/shift/super) are held during a click."
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["move", "click", "double_click", "right_click", "drag", "scroll"]},
                "x": {"type": "number"}, "y": {"type": "number"},
                "to_x": {"type": "number", "description": "drag destination"}, "to_y": {"type": "number"},
                "dx": {"type": "integer", "description": "scroll: horizontal clicks"}, "dy": {"type": "integer", "description": "scroll: vertical clicks"},
                "display": {"type": "integer", "default": 0},
                "modifiers": {"type": "array", "items": {"type": "string"}, "description": "held during click, e.g. [\"ctrl\"], [\"shift\"]"},
            },
            "required": ["action"],
        })
    }
    fn call<'a>(&'a self, a: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let d = self.0.as_ref();
            let action = arg_str(a, "action")
                .ok_or_else(|| RpcError::invalid_params("action is required"))?;
            let display = arg_i64(a, "display").unwrap_or(0).max(0) as usize;
            let mut mods = vec![];
            for m in a
                .get("modifiers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = m.as_str().unwrap_or("").to_lowercase();
                mods.push(modifier(&name).ok_or_else(|| fail(format!("unknown modifier {m}")))?);
            }
            let point = || -> Result<(f64, f64), ToolFail> {
                match (arg_f64(a, "x"), arg_f64(a, "y")) {
                    (Some(x), Some(y)) => Ok((x, y)),
                    _ => Err(RpcError::invalid_params(format!(
                        "x and y are required for {action}"
                    ))
                    .into()),
                }
            };

            match action {
                "move" => {
                    let (x, y) = point()?;
                    d.pointer_move(display, x, y).await.map_err(fail)?;
                    Ok(ToolResult::text(
                        format!("moved to ({}, {})", x as i64, y as i64),
                        None,
                    ))
                }
                "click" | "double_click" | "right_click" => {
                    let (x, y) = point()?;
                    let button = if action == "right_click" {
                        Button::Right
                    } else {
                        Button::Left
                    };
                    d.pointer_move(display, x, y).await.map_err(fail)?;
                    sleep_ms(30).await;
                    for m in &mods {
                        d.key(Key::Named(*m), true).await.map_err(fail)?;
                    }
                    let clicks = if action == "double_click" { 2 } else { 1 };
                    let r = async {
                        for i in 0..clicks {
                            d.button(button, true).await?;
                            sleep_ms(20).await;
                            d.button(button, false).await?;
                            if i + 1 < clicks {
                                sleep_ms(60).await;
                            }
                        }
                        Ok::<(), String>(())
                    }
                    .await;
                    for m in mods.iter().rev() {
                        let _ = d.key(Key::Named(*m), false).await;
                    }
                    r.map_err(fail)?;
                    let with = if mods.is_empty() {
                        ""
                    } else {
                        " with modifiers"
                    };
                    Ok(ToolResult::text(
                        format!("{action} at ({}, {}){with}", x as i64, y as i64),
                        None,
                    ))
                }
                "drag" => {
                    let (x, y) = point()?;
                    let (tx, ty) = match (arg_f64(a, "to_x"), arg_f64(a, "to_y")) {
                        (Some(tx), Some(ty)) => (tx, ty),
                        _ => {
                            return Err(RpcError::invalid_params(
                                "to_x and to_y are required for drag",
                            )
                            .into())
                        }
                    };
                    d.pointer_move(display, x, y).await.map_err(fail)?;
                    sleep_ms(30).await;
                    d.button(Button::Left, true).await.map_err(fail)?;
                    let r = async {
                        const STEPS: i32 = 12;
                        for i in 1..=STEPS {
                            let t = i as f64 / STEPS as f64;
                            d.pointer_move(display, x + (tx - x) * t, y + (ty - y) * t)
                                .await?;
                            sleep_ms(15).await;
                        }
                        Ok::<(), String>(())
                    }
                    .await;
                    // Release even if a step failed, so the button is never left held.
                    let up = d.button(Button::Left, false).await;
                    r.and(up).map_err(fail)?;
                    Ok(ToolResult::text(
                        format!(
                            "dragged ({}, {}) → ({}, {})",
                            x as i64, y as i64, tx as i64, ty as i64
                        ),
                        None,
                    ))
                }
                "scroll" => {
                    let dx = arg_i64(a, "dx").unwrap_or(0) as i32;
                    let dy = arg_i64(a, "dy").unwrap_or(0) as i32;
                    if dx == 0 && dy == 0 {
                        return Err(RpcError::invalid_params("scroll needs dx or dy").into());
                    }
                    if a.get("x").is_some() {
                        let (x, y) = point()?;
                        d.pointer_move(display, x, y).await.map_err(fail)?;
                        sleep_ms(20).await;
                    }
                    d.scroll(dx, dy).await.map_err(fail)?;
                    Ok(ToolResult::text(format!("scrolled dx={dx} dy={dy}"), None))
                }
                other => Err(RpcError::invalid_params(format!("unknown action {other}")).into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::desktop::{Capture, DesktopFuture, Display};
    use std::sync::Mutex;

    /// Records every event; capture is a 2-display layout at 2 px/pt with a marked pixel.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Desktop for Recorder {
        fn mechanism(&self) -> &'static str {
            "test"
        }
        fn capture(&self) -> DesktopFuture<'_, Capture> {
            Box::pin(async {
                // Layout: display 0 = 100×50 pt at (0,0); display 1 = 50×50 pt at (100,0).
                let mut image = image::RgbaImage::new(300, 100);
                image.put_pixel(220, 20, image::Rgba([255, 0, 0, 255])); // display 1, point (10,10)
                Ok(Capture {
                    image,
                    pixels_per_point: 2.0,
                    displays: vec![
                        Display {
                            index: 0,
                            x: 0.0,
                            y: 0.0,
                            width: 100.0,
                            height: 50.0,
                            scale: 2.0,
                        },
                        Display {
                            index: 1,
                            x: 100.0,
                            y: 0.0,
                            width: 50.0,
                            height: 50.0,
                            scale: 2.0,
                        },
                    ],
                })
            })
        }
        fn pointer_move(&self, display: usize, x: f64, y: f64) -> DesktopFuture<'_, ()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("move {display} {x} {y}"));
            Box::pin(async { Ok(()) })
        }
        fn button(&self, b: Button, down: bool) -> DesktopFuture<'_, ()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{b:?} {}", if down { "down" } else { "up" }));
            Box::pin(async { Ok(()) })
        }
        fn scroll(&self, dx: i32, dy: i32) -> DesktopFuture<'_, ()> {
            self.0.lock().unwrap().push(format!("scroll {dx} {dy}"));
            Box::pin(async { Ok(()) })
        }
        fn key(&self, k: Key, down: bool) -> DesktopFuture<'_, ()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{k:?} {}", if down { "down" } else { "up" }));
            Box::pin(async { Ok(()) })
        }
        fn status(&self) -> String {
            String::new()
        }
        fn facts(&self) -> crate::platform::desktop::DesktopFacts {
            Default::default()
        }
    }

    fn rec() -> Arc<Recorder> {
        Arc::new(Recorder::default())
    }
    fn events(r: &Recorder) -> Vec<String> {
        r.0.lock().unwrap().clone()
    }

    #[test]
    fn combos_parse() {
        let c = parse_combo("Ctrl+Shift+T").unwrap();
        assert_eq!(
            c,
            Combo {
                mods: vec![NamedKey::Ctrl, NamedKey::Shift],
                key: Key::Char('t')
            }
        );
        assert_eq!(parse_combo("cmd+c").unwrap().mods, vec![NamedKey::Ctrl]);
        assert_eq!(
            parse_combo("return").unwrap().key,
            Key::Named(NamedKey::Return)
        );
        assert_eq!(parse_combo("f12").unwrap().key, Key::Named(NamedKey::F(12)));
        assert_eq!(parse_combo("ctrl+plus").unwrap().key, Key::Char('+'));
        assert!(parse_combo("hyper+x").unwrap_err().contains("modifier"));
        assert!(parse_combo("ctrl+nope")
            .unwrap_err()
            .contains("unknown key"));
        assert!(parse_combo("ctrl+").is_err());
    }

    #[tokio::test]
    async fn screenshot_maps_points_to_pixels_per_display() {
        let t = ScreenshotTool(rec());
        let r = t
            .call(&json!({"display": 1, "format": "png"}))
            .await
            .ok()
            .unwrap();
        let s = r.structured.unwrap();
        assert_eq!(
            (s["image"]["width"].as_u64(), s["image"]["height"].as_u64()),
            (Some(50), Some(50))
        );
        // scale 2 on a 10×10 pt region around the marked point → 20×20 px, marked pixel at (0,0).
        let r = t
            .call(&json!({"display": 1, "format": "png", "scale": 2, "region": {"x": 10, "y": 10, "width": 10, "height": 10}}))
            .await
            .ok()
            .unwrap();
        let Content::Image { data_b64, .. } = &r.content[0] else {
            panic!()
        };
        let png = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (20, 20));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert!(t.call(&json!({"display": 5})).await.is_err());
        assert!(t
            .call(&json!({"region": {"x": 500, "y": 0, "width": 10, "height": 10}}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn click_with_modifiers_releases_in_reverse() {
        let r = rec();
        MouseTool(r.clone())
            .call(&json!({"action": "click", "x": 5, "y": 6, "modifiers": ["ctrl", "shift"]}))
            .await
            .ok()
            .unwrap();
        assert_eq!(
            events(&r),
            vec![
                "move 0 5 6",
                "Named(Ctrl) down",
                "Named(Shift) down",
                "Left down",
                "Left up",
                "Named(Shift) up",
                "Named(Ctrl) up"
            ]
        );
    }

    #[tokio::test]
    async fn drag_scroll_and_typing() {
        let r = rec();
        let m = MouseTool(r.clone());
        m.call(&json!({"action": "drag", "x": 0, "y": 0, "to_x": 12, "to_y": 24}))
            .await
            .ok()
            .unwrap();
        let ev = events(&r);
        assert_eq!(ev.first().unwrap(), "move 0 0 0");
        assert_eq!(ev[1], "Left down");
        assert_eq!(ev[ev.len() - 2], "move 0 12 24");
        assert_eq!(ev.last().unwrap(), "Left up");

        let r = rec();
        MouseTool(r.clone())
            .call(&json!({"action": "scroll", "dy": -3}))
            .await
            .ok()
            .unwrap();
        assert_eq!(events(&r), vec!["scroll 0 -3"]);
        assert!(MouseTool(rec())
            .call(&json!({"action": "scroll"}))
            .await
            .is_err());

        let r = rec();
        KeyTool(r.clone())
            .call(&json!({"action": "type", "text": "a\n中", "delay_ms": 0}))
            .await
            .ok()
            .unwrap();
        assert_eq!(
            events(&r),
            vec![
                "Char('a') down",
                "Char('a') up",
                "Named(Return) down",
                "Named(Return) up",
                "Char('中') down",
                "Char('中') up"
            ]
        );
    }
}
