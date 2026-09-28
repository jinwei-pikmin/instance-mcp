//! `Desktop` for wlroots compositors (labwc, sway, wayfire) via the seat's CLI tools:
//! `grim` (wlr-screencopy), `wlrctl` (virtual-pointer) and `wtype` (virtual-keyboard).
//!
//! Every helper is run with `WAYLAND_DISPLAY` / `XDG_RUNTIME_DIR` defaulted to the seat's
//! usual values, because a systemd unit or an ssh-started daemon has neither.

use std::process::Command;

use crate::platform::desktop::{Button, Capture, Combo, Desktop, DesktopError};

pub struct Wlroots;

extern "C" {
    fn getuid() -> u32;
}

fn seat_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        cmd.env("WAYLAND_DISPLAY", "wayland-0");
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        // SAFETY: getuid has no preconditions and cannot fail.
        cmd.env(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", unsafe { getuid() }),
        );
    }
    cmd
}

fn run_seat(bin: &str, args: &[String]) -> Result<(), DesktopError> {
    let out = seat_command(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{bin} {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn sv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn grim_command(scale: f64, format: &str, quality: i64) -> Command {
    let mut cmd = seat_command("grim");
    cmd.arg("-s").arg(format!("{scale}")).arg("-t").arg(format);
    if format == "jpeg" {
        cmd.arg("-q").arg(quality.to_string());
    }
    cmd.arg("-"); // stdout
    cmd
}

fn button_name(b: Button) -> &'static str {
    match b {
        Button::Left => "left",
        Button::Right => "right",
    }
}

fn i(v: f64) -> String {
    format!("{}", v.round() as i64)
}

impl Desktop for Wlroots {
    fn display_ok(&self) -> bool {
        grim_command(0.05, "png", 80)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn capture(&self, scale: f64, format: &str, quality: i64) -> Result<Capture, DesktopError> {
        let mut out = grim_command(scale, format, quality)
            .output()
            .map_err(|e| format!("grim: {e}"))?;
        let mut produced: &'static str = if format == "jpeg" { "jpeg" } else { "png" };
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if format == "jpeg" && err.contains("jpeg support disabled") {
                // Debian's grim is built without libjpeg; callers (Connect asks for
                // jpeg) decode by content, so hand back PNG instead of failing.
                produced = "png";
                out = grim_command(scale, "png", quality)
                    .output()
                    .map_err(|e| format!("grim: {e}"))?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    return Err(format!("grim failed ({}): {err}", out.status));
                }
            } else {
                return Err(format!("grim failed ({}): {err}", out.status));
            }
        }
        Ok(Capture {
            bytes: out.stdout,
            format: produced,
        })
    }

    /// wlrctl only knows relative motion: pin to the top-left corner, then move by (x, y).
    /// Correct on a single output; wrong with several.
    fn pointer_goto(&self, x: f64, y: f64) -> Result<(), DesktopError> {
        run_seat("wlrctl", &sv(&["pointer", "move", "-20000", "-20000"]))?;
        run_seat("wlrctl", &sv(&["pointer", "move", &i(x), &i(y)]))
    }

    fn pointer_move_rel(&self, dx: f64, dy: f64) -> Result<(), DesktopError> {
        run_seat("wlrctl", &sv(&["pointer", "move", &i(dx), &i(dy)]))
    }

    fn pointer_click(&self, button: Button) -> Result<(), DesktopError> {
        run_seat("wlrctl", &sv(&["pointer", "click", button_name(button)]))
    }

    fn pointer_press(&self, button: Button) -> Result<(), DesktopError> {
        run_seat("wlrctl", &sv(&["pointer", "press", button_name(button)]))
    }

    fn pointer_release(&self, button: Button) -> Result<(), DesktopError> {
        run_seat("wlrctl", &sv(&["pointer", "release", button_name(button)]))
    }

    fn pointer_scroll(&self, dx: f64, dy: f64) -> Result<(), DesktopError> {
        run_seat("wlrctl", &sv(&["pointer", "scroll", &i(dy), &i(dx)]))
    }

    fn key_type(&self, text: &str) -> Result<(), DesktopError> {
        // `--` so text starting with '-' is not parsed as a flag.
        run_seat("wtype", &sv(&["--", text]))
    }

    fn key_press(&self, combo: &Combo) -> Result<(), DesktopError> {
        let mut args: Vec<String> = Vec::new();
        for m in &combo.modifiers {
            args.push("-M".into());
            args.push(m.clone());
        }
        args.push("-k".into());
        args.push(combo.key.clone());
        for m in combo.modifiers.iter().rev() {
            args.push("-m".into());
            args.push(m.clone());
        }
        run_seat("wtype", &args)
    }
}
