//! The graphical half of a backend: see the screen, move the pointer, press keys. The
//! `screenshot` / `mouse` / `key` tools are written against this trait only, so each OS (and
//! each Linux compositor family) plugs in its own implementation.
//!
//! Coordinates are *points*: the desktop's logical layout, the same space on every backend.
//! On a 2× (or fractional) HiDPI screen a point is several physical pixels; `screenshot` at
//! scale 1.0 returns one image pixel per point so a model can click what it sees.

use std::future::Future;
use std::pin::Pin;

use image::RgbaImage;

pub type DesktopFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// One monitor in the logical layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    /// Index callers pass as `display` (0 = the display at the layout origin).
    pub index: usize,
    /// Top-left in the global logical layout, points.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A full-desktop capture plus how its pixels map to points.
pub struct Capture {
    /// The whole layout (all displays), physical pixels.
    pub image: RgbaImage,
    /// Physical pixels per point (e.g. 1.6667 for 5/3 fractional scaling).
    pub pixels_per_point: f64,
    pub displays: Vec<Display>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// A key as the tools understand it; backends translate to their own codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Return,
    Tab,
    Space,
    Backspace,
    Delete,
    Escape,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    F(u8),
    CapsLock,
    PrintScreen,
    Menu,
    VolumeUp,
    VolumeDown,
    Mute,
    Shift,
    Ctrl,
    Alt,
    Super,
}

pub trait Desktop: Send + Sync {
    /// Human name of the mechanism, for `sys_info` ("xdg-desktop-portal").
    fn mechanism(&self) -> &'static str;

    /// The whole layout plus its displays. May need the human's one-time consent on first use.
    fn capture(&self) -> DesktopFuture<'_, Capture>;

    /// Absolute move to `(x, y)` points relative to `display`'s top-left.
    fn pointer_move(&self, display: usize, x: f64, y: f64) -> DesktopFuture<'_, ()>;

    fn button(&self, button: Button, down: bool) -> DesktopFuture<'_, ()>;

    /// Wheel clicks; positive `dy` scrolls content up (wheel away from you), matching the
    /// macOS tool's convention. Positive `dx` scrolls content left.
    fn scroll(&self, dx: i32, dy: i32) -> DesktopFuture<'_, ()>;

    fn key(&self, key: Key, down: bool) -> DesktopFuture<'_, ()>;

    /// Type one character of text (not a shortcut). Default: press and release it as a
    /// key; backends override when some characters cannot be sent as key events.
    fn type_char(&self, c: char) -> DesktopFuture<'_, ()> {
        Box::pin(async move {
            self.key(Key::Char(c), true).await?;
            self.key(Key::Char(c), false).await
        })
    }

    /// One-line status for `sys_info` without triggering a consent prompt.
    fn status(&self) -> String;
}
