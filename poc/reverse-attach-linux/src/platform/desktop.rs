//! The desktop a hands node can see and touch, as a trait so the same tools run on any
//! backend: wlroots (grim/wlrctl/wtype, here), xdg-desktop-portal (GNOME/KDE, later),
//! ScreenCaptureKit/CGEvent (macOS, later).
//!
//! Coordinates are display pixels = screenshot pixels at scale 1.

pub type DesktopError = String;

/// A captured frame, already encoded.
pub struct Capture {
    pub bytes: Vec<u8>,
    /// `png` or `jpeg` — the format actually produced, which may differ from the one
    /// requested when the backend cannot encode it.
    pub format: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// A key combo already split into normalised modifiers and one named key.
pub struct Combo {
    /// `ctrl`, `shift`, `alt`, `logo`, `altgr` — backend-neutral names.
    pub modifiers: Vec<String>,
    /// xkb key name (`Return`, `Escape`, `a`, `F4`).
    pub key: String,
}

pub trait Desktop: Send + Sync {
    /// Whether a display is reachable right now (cheap probe). Drives
    /// `sys_info.displays` / `permissions.screen_recording`.
    fn display_ok(&self) -> bool;

    /// Capture the whole display at `scale` (output px per display px) in `format`
    /// (`png` | `jpeg`) at JPEG `quality` 1–100.
    fn capture(&self, scale: f64, format: &str, quality: i64) -> Result<Capture, DesktopError>;

    /// Absolute pointer move.
    fn pointer_goto(&self, x: f64, y: f64) -> Result<(), DesktopError>;
    /// Relative pointer move.
    fn pointer_move_rel(&self, dx: f64, dy: f64) -> Result<(), DesktopError>;
    fn pointer_click(&self, button: Button) -> Result<(), DesktopError>;
    fn pointer_press(&self, button: Button) -> Result<(), DesktopError>;
    fn pointer_release(&self, button: Button) -> Result<(), DesktopError>;
    /// Positive `dy` scrolls down, positive `dx` scrolls right.
    fn pointer_scroll(&self, dx: f64, dy: f64) -> Result<(), DesktopError>;

    /// Type text (unicode, layout independent).
    fn key_type(&self, text: &str) -> Result<(), DesktopError>;
    /// Press and release a combo.
    fn key_press(&self, combo: &Combo) -> Result<(), DesktopError>;
}
