//! Input devices that live as long as the daemon, on the wlroots seat.
//!
//! Why this exists: a seat with no physical input (black's headless sway, a Pi with no
//! mouse or keyboard) advertises no pointer/keyboard capability, so clients such as Chromium
//! hold no `wl_pointer`/`wl_keyboard`. `wlrctl` and `wtype` each create a virtual device,
//! send their events and exit within milliseconds: the capability flickers on, the client's
//! `get_pointer`/`get_keyboard` round-trip loses the race, and every event is dropped — while
//! the tool exits 0 and the MCP call reports `ok: true`. Measured on black (2026-09-29):
//! a transient `wtype` was ignored; the same keys typed while another virtual keyboard stayed
//! connected arrived.
//!
//! So the node keeps both capabilities up permanently:
//! - **pointer**: its own Wayland connection holding one `zwlr_virtual_pointer_v1`, which also
//!   carries every pointer event (absolute motion, so `wlrctl`'s pin-to-corner-and-move-
//!   relative hack and its multi-output error are gone);
//! - **keyboard**: a long-lived `wtype -s` anchor child. Keys are still typed by `wtype`,
//!   which builds a keymap per string — unicode typing stays layout independent.
//!
//! The Wayland client is a few hand-encoded requests over the socket rather than a crate: the
//! node needs exactly three interfaces and no fd passing.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::platform::desktop::{Button, DesktopError};

/// How long a freshly created device is given before its first event, so clients can see
/// the new capability and bind `wl_pointer` / `wl_keyboard`. 1.5 s worked on black; 400 ms
/// is ample for one round-trip on a local socket.
const SETTLE: Duration = Duration::from_millis(400);

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// One wheel notch, in the axis units libinput reports for a real mouse (15°).
const NOTCH: f64 = 15.0;

extern "C" {
    fn getuid() -> u32;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
}
const PR_SET_PDEATHSIG: i32 = 1;
const SIGTERM: u64 = 15;

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

fn now_ms() -> u32 {
    epoch().elapsed().as_millis() as u32
}

/// Same defaults as the CLI helpers: a systemd unit or ssh-started daemon may have neither.
pub(crate) fn wayland_socket() -> PathBuf {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    if display.starts_with('/') {
        return PathBuf::from(display);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        // SAFETY: getuid has no preconditions and cannot fail.
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { getuid() }));
    PathBuf::from(runtime).join(display)
}

// ---------------------------------------------------------------------------------------
// Wire format: little-endian u32 words; header = object id, then (size << 16 | opcode).

fn fixed(v: f64) -> u32 {
    ((v * 256.0).round() as i32) as u32
}

pub(crate) struct Msg(Vec<u8>);

impl Msg {
    pub(crate) fn new() -> Self {
        Msg(Vec::new())
    }
    pub(crate) fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn str(mut self, s: &str) -> Self {
        let len = s.len() as u32 + 1; // includes NUL
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
        self
    }
    pub(crate) fn encode(self, object: u32, opcode: u16) -> Vec<u8> {
        let size = (8 + self.0.len()) as u32;
        let mut out = Vec::with_capacity(size as usize);
        out.extend_from_slice(&object.to_le_bytes());
        out.extend_from_slice(&((size << 16) | opcode as u32).to_le_bytes());
        out.extend_from_slice(&self.0);
        out
    }
}

pub(crate) struct Event {
    pub object: u32,
    pub opcode: u16,
    pub body: Vec<u8>,
}

fn read_event(r: &mut impl Read) -> std::io::Result<Event> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head)?;
    let object = u32::from_le_bytes(head[0..4].try_into().unwrap());
    let word = u32::from_le_bytes(head[4..8].try_into().unwrap());
    let size = (word >> 16) as usize;
    if size < 8 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "short wayland message",
        ));
    }
    let mut body = vec![0u8; size - 8];
    r.read_exact(&mut body)?;
    Ok(Event {
        object,
        opcode: (word & 0xffff) as u16,
        body,
    })
}

/// Cursor over an event body.
pub(crate) struct Args<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Args<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Args { b, at: 0 }
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        let v = self.b.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_le_bytes(v.try_into().ok()?))
    }
    pub(crate) fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    pub(crate) fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Some(String::new());
        }
        let bytes = self.b.get(self.at..self.at + len)?;
        self.at += (len + 3) & !3;
        Some(String::from_utf8_lossy(&bytes[..len - 1]).into_owned())
    }
}

// ---------------------------------------------------------------------------------------
// Output layout, from wl_output geometry/mode/scale, so absolute motion maps screenshot
// pixels (grim -s 1 = layout coordinates) onto the whole layout.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct OutputInfo {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
}

/// (origin x, origin y, extent w, extent h) of the union of outputs, in layout coordinates.
pub(crate) fn layout_box(outputs: &[OutputInfo]) -> Option<(i32, i32, u32, u32)> {
    let logical: Vec<(i32, i32, i32, i32)> = outputs
        .iter()
        .filter(|o| o.width > 0 && o.height > 0)
        .map(|o| {
            let s = o.scale.max(1);
            (
                o.x,
                o.y,
                o.x + (o.width + s - 1) / s,
                o.y + (o.height + s - 1) / s,
            )
        })
        .collect();
    let min_x = logical.iter().map(|l| l.0).min()?;
    let min_y = logical.iter().map(|l| l.1).min()?;
    let max_x = logical.iter().map(|l| l.2).max()?;
    let max_y = logical.iter().map(|l| l.3).max()?;
    Some((min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32))
}

/// Screenshot pixel → the (x, y, x_extent, y_extent) that `motion_absolute` wants, clamped
/// inside the layout.
pub(crate) fn absolute(x: f64, y: f64, layout: (i32, i32, u32, u32)) -> (u32, u32, u32, u32) {
    let (ox, oy, w, h) = layout;
    let cx = (x - ox as f64)
        .round()
        .clamp(0.0, w.saturating_sub(1) as f64) as u32;
    let cy = (y - oy as f64)
        .round()
        .clamp(0.0, h.saturating_sub(1) as f64) as u32;
    (cx, cy, w, h)
}

// ---------------------------------------------------------------------------------------

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;

struct Pointer {
    sock: UnixStream,
    id: u32,
    layout: (i32, i32, u32, u32),
    dead: Arc<AtomicBool>,
}

impl Pointer {
    fn connect() -> Result<Pointer, DesktopError> {
        let path = wayland_socket();
        let mut sock =
            UnixStream::connect(&path).map_err(|e| format!("wayland {}: {e}", path.display()))?;
        sock.set_read_timeout(Some(Duration::from_secs(3))).ok();

        let mut next = 3u32;
        let mut alloc = || {
            let id = next;
            next += 1;
            id
        };
        let send = |sock: &mut UnixStream, bytes: Vec<u8>| {
            sock.write_all(&bytes)
                .map_err(|e| format!("wayland write: {e}"))
        };

        // Registry, then a sync to know when the global list is complete.
        send(&mut sock, Msg::new().u32(REGISTRY).encode(DISPLAY, 1))?;
        let sync1 = alloc();
        send(&mut sock, Msg::new().u32(sync1).encode(DISPLAY, 0))?;
        let mut manager: Option<(u32, u32)> = None; // (name, version)
        let mut output_names: Vec<(u32, u32)> = Vec::new();
        loop {
            let ev = read_event(&mut sock).map_err(|e| format!("wayland read: {e}"))?;
            match (ev.object, ev.opcode) {
                (DISPLAY, 0) => return Err(display_error(&ev.body)),
                (REGISTRY, 0) => {
                    let mut a = Args::new(&ev.body);
                    let (Some(name), Some(iface), Some(ver)) = (a.u32(), a.str(), a.u32()) else {
                        continue;
                    };
                    match iface.as_str() {
                        "zwlr_virtual_pointer_manager_v1" => manager = Some((name, ver)),
                        "wl_output" => output_names.push((name, ver)),
                        _ => {}
                    }
                }
                (id, 0) if id == sync1 => break,
                _ => {}
            }
        }
        let (mname, _) = manager.ok_or_else(|| {
            "compositor has no zwlr_virtual_pointer_manager_v1 (wlroots virtual-pointer)"
                .to_string()
        })?;

        let mgr = alloc();
        send(
            &mut sock,
            Msg::new()
                .u32(mname)
                .str("zwlr_virtual_pointer_manager_v1")
                .u32(1)
                .u32(mgr)
                .encode(REGISTRY, 0),
        )?;
        let mut outputs: Vec<(u32, OutputInfo)> = Vec::new();
        for (name, ver) in &output_names {
            let id = alloc();
            send(
                &mut sock,
                Msg::new()
                    .u32(*name)
                    .str("wl_output")
                    .u32((*ver).min(2))
                    .u32(id)
                    .encode(REGISTRY, 0),
            )?;
            outputs.push((
                id,
                OutputInfo {
                    scale: 1,
                    ..Default::default()
                },
            ));
        }
        let id = alloc();
        // create_virtual_pointer(seat = null → the default seat, id)
        send(&mut sock, Msg::new().u32(0).u32(id).encode(mgr, 0))?;
        let sync2 = alloc();
        send(&mut sock, Msg::new().u32(sync2).encode(DISPLAY, 0))?;
        loop {
            let ev = read_event(&mut sock).map_err(|e| format!("wayland read: {e}"))?;
            if ev.object == DISPLAY && ev.opcode == 0 {
                return Err(display_error(&ev.body));
            }
            if ev.object == sync2 && ev.opcode == 0 {
                break;
            }
            if let Some((_, o)) = outputs.iter_mut().find(|(oid, _)| *oid == ev.object) {
                let mut a = Args::new(&ev.body);
                match ev.opcode {
                    0 => {
                        // geometry: x, y, physical w, physical h, subpixel, make, model, transform
                        if let (Some(x), Some(y)) = (a.i32(), a.i32()) {
                            o.x = x;
                            o.y = y;
                        }
                    }
                    1 => {
                        // mode: flags, width, height, refresh — keep the current one (flag 1)
                        if let (Some(flags), Some(w), Some(h)) = (a.u32(), a.i32(), a.i32()) {
                            if flags & 1 != 0 {
                                o.width = w;
                                o.height = h;
                            }
                        }
                    }
                    3 => {
                        if let Some(s) = a.i32() {
                            o.scale = s.max(1);
                        }
                    }
                    _ => {}
                }
            }
        }
        let infos: Vec<OutputInfo> = outputs.iter().map(|(_, o)| *o).collect();
        let layout =
            layout_box(&infos).ok_or_else(|| "compositor reports no outputs".to_string())?;

        // Drain whatever the compositor sends from now on (output changes, delete_id) so its
        // buffer never fills; EOF or a protocol error marks the device dead → reconnect.
        let dead = Arc::new(AtomicBool::new(false));
        let mut reader = sock
            .try_clone()
            .map_err(|e| format!("wayland clone: {e}"))?;
        reader.set_read_timeout(None).ok();
        let flag = dead.clone();
        thread::spawn(move || loop {
            match read_event(&mut reader) {
                Ok(ev) if ev.object == DISPLAY && ev.opcode == 0 => {
                    eprintln!("seat: virtual pointer {}", display_error(&ev.body));
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
            }
        });

        eprintln!("seat: virtual pointer up, layout {layout:?}");
        thread::sleep(SETTLE);
        Ok(Pointer {
            sock,
            id,
            layout,
            dead,
        })
    }

    fn send(&mut self, frames: &[Vec<u8>]) -> Result<(), DesktopError> {
        let mut all = Vec::new();
        for f in frames {
            all.extend_from_slice(f);
        }
        self.sock
            .write_all(&all)
            .map_err(|e| format!("wayland write: {e}"))
    }

    fn frame(&self) -> Vec<u8> {
        Msg::new().encode(self.id, 4)
    }
}

fn display_error(body: &[u8]) -> String {
    let mut a = Args::new(body);
    let (_obj, code, msg) = (a.u32(), a.u32(), a.str());
    format!(
        "wayland protocol error {}: {}",
        code.unwrap_or(0),
        msg.unwrap_or_default()
    )
}

fn pointer_slot() -> &'static Mutex<Option<Pointer>> {
    static P: OnceLock<Mutex<Option<Pointer>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Run `f` on a live virtual pointer, (re)connecting when there is none or it died. One
/// retry on a write failure: the compositor may have restarted under us.
fn with_pointer(f: impl Fn(&mut Pointer) -> Result<(), DesktopError>) -> Result<(), DesktopError> {
    let mut slot = pointer_slot().lock().unwrap_or_else(|p| p.into_inner());
    for attempt in 0..2 {
        if slot.as_ref().is_none_or(|p| p.dead.load(Ordering::SeqCst)) {
            *slot = Some(Pointer::connect()?);
        }
        match f(slot.as_mut().unwrap()) {
            Ok(()) => return Ok(()),
            Err(e) if attempt == 0 => {
                eprintln!("seat: {e}; reconnecting");
                *slot = None;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn button_code(b: Button) -> u32 {
    match b {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
    }
}

pub fn pointer_goto(x: f64, y: f64) -> Result<(), DesktopError> {
    with_pointer(|p| {
        let (ax, ay, w, h) = absolute(x, y, p.layout);
        let motion = Msg::new()
            .u32(now_ms())
            .u32(ax)
            .u32(ay)
            .u32(w)
            .u32(h)
            .encode(p.id, 1);
        let frame = p.frame();
        p.send(&[motion, frame])
    })
}

pub fn pointer_move_rel(dx: f64, dy: f64) -> Result<(), DesktopError> {
    with_pointer(|p| {
        let motion = Msg::new()
            .u32(now_ms())
            .u32(fixed(dx))
            .u32(fixed(dy))
            .encode(p.id, 0);
        let frame = p.frame();
        p.send(&[motion, frame])
    })
}

fn button(b: Button, pressed: bool) -> Result<(), DesktopError> {
    with_pointer(|p| {
        let ev = Msg::new()
            .u32(now_ms())
            .u32(button_code(b))
            .u32(pressed as u32)
            .encode(p.id, 2);
        let frame = p.frame();
        p.send(&[ev, frame])
    })
}

pub fn pointer_press(b: Button) -> Result<(), DesktopError> {
    button(b, true)
}

pub fn pointer_release(b: Button) -> Result<(), DesktopError> {
    button(b, false)
}

pub fn pointer_click(b: Button) -> Result<(), DesktopError> {
    button(b, true)?;
    thread::sleep(Duration::from_millis(30));
    button(b, false)
}

/// `dy`/`dx` in wheel notches (lines), positive = down/right.
pub fn pointer_scroll(dx: f64, dy: f64) -> Result<(), DesktopError> {
    with_pointer(|p| {
        let mut frames = vec![Msg::new().u32(0 /* wheel */).encode(p.id, 5)];
        for (axis, n) in [(0u32, dy), (1u32, dx)] {
            let notches = n.round() as i32;
            if notches != 0 {
                // axis_discrete(time, axis, value, discrete)
                frames.push(
                    Msg::new()
                        .u32(now_ms())
                        .u32(axis)
                        .u32(fixed(notches as f64 * NOTCH))
                        .u32(notches as u32)
                        .encode(p.id, 7),
                );
            }
        }
        frames.push(p.frame());
        p.send(&frames)
    })
}

// ---------------------------------------------------------------------------------------
// Keyboard anchor: a `wtype` that only sleeps, keeping one virtual keyboard connected.

fn anchor_slot() -> &'static Mutex<Option<Child>> {
    static K: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    K.get_or_init(|| Mutex::new(None))
}

/// `wtype -s` multiplies milliseconds by 1000 in a C `int`: anything above ~2147 s wraps
/// negative and wtype exits at once (seen on black with 3600000). 30 minutes, respawned.
const ANCHOR_MS: &str = "1800000";

/// Make sure the keyboard capability is up before `wtype` types. Respawns the anchor when it
/// has exited.
pub fn ensure_keyboard(mut seat_command: impl FnMut(&str) -> Command) -> Result<(), DesktopError> {
    let mut slot = anchor_slot().lock().unwrap_or_else(|p| p.into_inner());
    let alive = match slot.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    };
    if alive {
        return Ok(());
    }
    let mut cmd = seat_command("wtype");
    cmd.args(["-s", ANCHOR_MS])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: prctl(PR_SET_PDEATHSIG) is async-signal-safe; it only asks the kernel to
    // SIGTERM the anchor when the daemon dies (the unit uses KillMode=process).
    unsafe {
        cmd.pre_exec(|| {
            prctl(PR_SET_PDEATHSIG, SIGTERM, 0, 0, 0);
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("wtype keyboard anchor: {e}"))?;
    eprintln!("seat: virtual keyboard anchor up (pid {})", child.id());
    *slot = Some(child);
    drop(slot);
    thread::sleep(SETTLE);
    Ok(())
}

/// Bring both devices up at startup, so the capabilities exist before the first tool call
/// (and before any client — a browser started later binds them on its own). Failures are
/// logged, not fatal: a node without a display still serves `bash`.
pub fn warm_up(mut seat_command: impl FnMut(&str) -> Command + Send + 'static) {
    thread::spawn(move || {
        if let Err(e) = with_pointer(|_| Ok(())) {
            eprintln!("seat: virtual pointer unavailable: {e}");
        }
        // Keep the keyboard anchor up between tool calls too, so the capability does not
        // drop when an anchor's sleep ends (clients would release wl_keyboard).
        let mut reported = false;
        loop {
            match ensure_keyboard(&mut seat_command) {
                Ok(()) => reported = false,
                Err(e) if !reported => {
                    eprintln!("seat: {e}");
                    reported = true;
                }
                Err(_) => {}
            }
            thread::sleep(Duration::from_secs(20));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_encode_header_size_and_opcode() {
        let b = Msg::new().u32(7).encode(9, 3);
        assert_eq!(b.len(), 12);
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 9);
        assert_eq!(
            u32::from_le_bytes(b[4..8].try_into().unwrap()),
            (12 << 16) | 3
        );
    }

    #[test]
    fn strings_are_nul_terminated_and_padded() {
        let b = Msg::new().str("wl_output").encode(2, 0);
        // header 8 + len 4 + "wl_output\0" (10) padded to 12
        assert_eq!(b.len(), 8 + 4 + 12);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 10);
        let mut a = Args::new(&b[8..]);
        assert_eq!(a.str().as_deref(), Some("wl_output"));
    }

    #[test]
    fn events_round_trip_through_the_reader() {
        let bytes = Msg::new()
            .u32(5)
            .str("zwlr_virtual_pointer_manager_v1")
            .u32(2)
            .encode(2, 0);
        let ev = read_event(&mut &bytes[..]).unwrap();
        assert_eq!((ev.object, ev.opcode), (2, 0));
        let mut a = Args::new(&ev.body);
        assert_eq!(a.u32(), Some(5));
        assert_eq!(a.str().as_deref(), Some("zwlr_virtual_pointer_manager_v1"));
        assert_eq!(a.u32(), Some(2));
    }

    #[test]
    fn fixed_point_is_24_8() {
        assert_eq!(fixed(1.0), 256);
        assert_eq!(fixed(-2.5) as i32, -640);
    }

    #[test]
    fn layout_is_the_union_of_logical_outputs() {
        let black = [OutputInfo {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1,
        }];
        assert_eq!(layout_box(&black), Some((0, 0, 1920, 1080)));
        let hidpi = [OutputInfo {
            x: 0,
            y: 0,
            width: 2880,
            height: 1800,
            scale: 2,
        }];
        assert_eq!(layout_box(&hidpi), Some((0, 0, 1440, 900)));
        let two = [
            OutputInfo {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1,
            },
            OutputInfo {
                x: 1920,
                y: 0,
                width: 1280,
                height: 1024,
                scale: 1,
            },
        ];
        assert_eq!(layout_box(&two), Some((0, 0, 3200, 1080)));
        assert_eq!(layout_box(&[]), None);
    }

    #[test]
    fn absolute_motion_maps_screenshot_pixels_and_clamps() {
        let l = (0, 0, 1920, 1080);
        assert_eq!(absolute(1246.0, 198.0, l), (1246, 198, 1920, 1080));
        assert_eq!(absolute(-50.0, 5000.0, l), (0, 1079, 1920, 1080));
        // A layout whose origin is not 0,0.
        assert_eq!(
            absolute(100.0, 100.0, (-1920, 0, 3840, 1080)),
            (2020, 100, 3840, 1080)
        );
    }
}
