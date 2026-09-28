//! `Desktop` over xdg-desktop-portal: works on GNOME and KDE Wayland (and X11) sessions,
//! where wlroots tools like grim/ydotool do not.
//!
//! - Screenshot: `org.freedesktop.portal.Screenshot`, non-interactive. The portal writes a
//!   PNG (GNOME: into ~/Pictures); we read it and delete that one file.
//! - Input and layout: one `org.freedesktop.portal.RemoteDesktop` session with keyboard +
//!   pointer and a ScreenCast source per monitor. We never consume the PipeWire streams; we
//!   only need each stream's node id (absolute pointer motion is stream-relative) and its
//!   logical position/size (the display layout).
//! - Consent: the first session shows the desktop's "remote desktop" dialog on this
//!   machine's screen; a human must approve with remote interaction on. We ask for
//!   `persist_mode = 2` and keep the restore token (single-use: rotated on every start) in
//!   `~/.config/oab-instance-mcp/portal-restore-token`, so later sessions start silently.
//! - The session is closed after `IDLE` without use, so the desktop's "being controlled"
//!   indicator does not stay up; the next call reopens it silently.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{Mutex, OnceCell};
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

use crate::log;
use crate::platform::desktop::{
    with_modifiers, Button, Capture, Desktop, DesktopFacts, DesktopFuture, Display, Key, NamedKey,
};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const RD: &str = "org.freedesktop.portal.RemoteDesktop";
const SC: &str = "org.freedesktop.portal.ScreenCast";
const SHOT: &str = "org.freedesktop.portal.Screenshot";
/// Long enough for a human to walk to the machine and click Allow.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);
/// Non-interactive screenshots need no human; if one hangs, something on the desktop is
/// blocking it (a modal dialog), and the caller should hear so quickly.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const IDLE: Duration = Duration::from_secs(300);
const UNICODE_ENTRY_PAUSE: Duration = Duration::from_millis(60);
/// evdev codes, as the RemoteDesktop portal expects.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;

#[derive(Debug, Clone, PartialEq)]
struct Stream {
    node: u32,
    /// Logical layout rectangle, points.
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Stream pixels per point for absolute motion. The portal reports logical sizes, but
    /// Mutter interprets NotifyPointerMotionAbsolute in the monitor's *physical* pixels, so on
    /// GNOME this is the monitor's scale (1.667 at 5/3 fractional scaling); 1.0 elsewhere.
    motion_scale: f64,
}

#[derive(Clone)]
struct Session {
    path: OwnedObjectPath,
    /// Sorted: the stream at the layout origin first, then by position.
    streams: Vec<Stream>,
}

struct Inner {
    conn: OnceCell<Connection>,
    session: Mutex<Option<Session>>,
    last_used: StdMutex<Instant>,
    token_path: PathBuf,
    reaper: std::sync::Once,
    /// Layout from the most recent session; kept after it closes so `sys_info` can report
    /// displays without opening (or waiting on) a session.
    last_layout: StdMutex<Vec<Display>>,
}

pub struct Portal(Arc<Inner>);

type Opts = HashMap<&'static str, Value<'static>>;

fn new_token() -> String {
    format!("oab{}", uuid::Uuid::new_v4().simple())
}

impl Portal {
    pub fn new(config_dir: PathBuf) -> Self {
        let inner = Arc::new(Inner {
            conn: OnceCell::new(),
            session: Mutex::new(None),
            last_used: StdMutex::new(Instant::now()),
            token_path: config_dir.join("portal-restore-token"),
            reaper: std::sync::Once::new(),
            last_layout: StdMutex::new(Vec::new()),
        });
        // Learn the layout up front so `sys_info` lists displays before any session: a
        // client like OpenAB Connect only takes screenshots of a machine that has displays.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let me = inner.clone();
            rt.spawn(async move { me.refresh_layout().await });
        }
        Portal(inner)
    }
}

impl Inner {
    async fn conn(&self) -> Result<&Connection, String> {
        self.conn
            .get_or_try_init(|| async { Connection::session().await })
            .await
            .map_err(|e| {
                format!("no D-Bus session bus (is this running inside the desktop session?): {e}")
            })
    }

    async fn proxy(&self, iface: &'static str) -> Result<Proxy<'static>, String> {
        let conn = self.conn().await?.clone();
        Proxy::new(&conn, DEST, PATH, iface)
            .await
            .map_err(|e| e.to_string())
    }

    /// Portal request/response. `build` gets the options map (already holding a fresh
    /// `handle_token`) and returns the call body. We subscribe to the Response signal on
    /// the request path derived from that token *before* calling, then wait for it.
    async fn request<B, F>(
        &self,
        iface: &'static str,
        method: &str,
        mut options: Opts,
        build: F,
        timeout: Duration,
    ) -> Result<HashMap<String, OwnedValue>, String>
    where
        B: serde::Serialize + DynamicType,
        F: FnOnce(Opts) -> B,
    {
        let conn = self.conn().await?;
        let token = new_token();
        let sender = conn
            .unique_name()
            .ok_or("no unique bus name")?
            .trim_start_matches(':')
            .replace('.', "_");
        let req_path = format!("{PATH}/request/{sender}/{token}");
        let req = Proxy::new(conn, DEST, req_path, "org.freedesktop.portal.Request")
            .await
            .map_err(|e| e.to_string())?;
        let mut responses = req
            .receive_signal("Response")
            .await
            .map_err(|e| e.to_string())?;
        options.insert("handle_token", Value::from(token));
        self.proxy(iface)
            .await?
            .call_method(method, &build(options))
            .await
            .map_err(|e| format!("{iface}.{method}: {e}"))?;
        let msg = tokio::time::timeout(timeout, responses.next())
            .await
            .map_err(|_| format!("{iface}.{method}: no answer within {}s", timeout.as_secs()))?
            .ok_or("portal request stream ended")?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg
            .body()
            .deserialize()
            .map_err(|e| format!("bad portal response: {e}"))?;
        match code {
            0 => Ok(results),
            1 => Err(format!("{method} was declined on this machine's screen")),
            _ => Err(format!(
                "{iface}.{method}: the portal failed (response {code})"
            )),
        }
    }

    fn load_token(&self) -> Option<String> {
        std::fs::read_to_string(&self.token_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn save_token(&self, t: Option<&str>) {
        let Some(t) = t else {
            let _ = std::fs::remove_file(&self.token_path);
            return;
        };
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.token_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.token_path);
        if let Ok(mut f) = f {
            let _ = f.write_all(t.as_bytes());
        }
    }

    async fn open_session(&self) -> Result<Session, String> {
        let mut o = Opts::new();
        o.insert("session_handle_token", Value::from(new_token()));
        let created = self
            .request(RD, "CreateSession", o, |o| (o,), Duration::from_secs(30))
            .await?;
        let path = created
            .get("session_handle")
            .and_then(|v| match &**v {
                Value::Str(s) => Some(s.to_string()),
                Value::ObjectPath(p) => Some(p.to_string()),
                _ => None,
            })
            .ok_or("CreateSession returned no session_handle")?;
        let session = OwnedObjectPath::try_from(path).map_err(|e| e.to_string())?;

        let mut o = Opts::new();
        o.insert("types", Value::from(3u32)); // keyboard | pointer
        o.insert("persist_mode", Value::from(2u32)); // until explicitly revoked
        let restore = self.load_token();
        if let Some(t) = &restore {
            o.insert("restore_token", Value::from(t.clone()));
        }
        let s = session.clone();
        self.request(
            RD,
            "SelectDevices",
            o,
            move |o| (s, o),
            Duration::from_secs(30),
        )
        .await?;

        let mut o = Opts::new();
        o.insert("types", Value::from(1u32)); // monitors
        o.insert("multiple", Value::from(true));
        let s = session.clone();
        self.request(
            SC,
            "SelectSources",
            o,
            move |o| (s, o),
            Duration::from_secs(30),
        )
        .await?;

        if restore.is_none() {
            log("desktop: asking for remote-desktop consent on this machine's screen");
        }
        let s = session.clone();
        let started = self
            .request(
                RD,
                "Start",
                Opts::new(),
                move |o| (s, "", o),
                CONSENT_TIMEOUT,
            )
            .await;
        let started = match started {
            Ok(r) => r,
            Err(e) => {
                // A stale or revoked token must not wedge us; the next try prompts afresh.
                self.save_token(None);
                return Err(e);
            }
        };
        let token = started
            .get("restore_token")
            .and_then(|v| <&str>::try_from(&**v).ok());
        self.save_token(token);

        let devices = started
            .get("devices")
            .and_then(|v| u32::try_from(&**v).ok())
            .unwrap_or(0);
        let mut streams = parse_streams(started.get("streams"));
        self.apply_motion_scales(&mut streams).await;
        let session = Session {
            path: session,
            streams,
        };
        if devices & 3 != 3 {
            self.close(&session).await;
            self.save_token(None);
            return Err(
                "remote control was not allowed in the desktop's dialog; call again and, on this \
                        machine's screen, turn on \"Allow Remote Interaction\" before sharing"
                    .into(),
            );
        }
        if session.streams.is_empty() {
            self.close(&session).await;
            return Err("the desktop shared no monitors".into());
        }
        log(&format!(
            "desktop: remote-desktop session open ({} display(s): {})",
            session.streams.len(),
            session
                .streams
                .iter()
                .map(|s| format!(
                    "{}x{}@{},{} scale {:.3}",
                    s.w, s.h, s.x, s.y, s.motion_scale
                ))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        Ok(session)
    }

    /// GNOME's logical monitors straight from Mutter: no consent, no session. None when
    /// not on GNOME.
    async fn mutter_layout(&self) -> Option<Result<Vec<Display>, String>> {
        let gnome = std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|d| d.to_ascii_uppercase().contains("GNOME"));
        if !gnome {
            return None;
        }
        Some(
            async {
                let conn = self.conn().await?;
                let p = Proxy::new(
                    conn,
                    "org.gnome.Mutter.DisplayConfig",
                    "/org/gnome/Mutter/DisplayConfig",
                    "org.gnome.Mutter.DisplayConfig",
                )
                .await
                .map_err(|e| e.to_string())?;
                let reply = p
                    .call_method("GetCurrentState", &())
                    .await
                    .map_err(|e| e.to_string())?;
                // (serial, monitors, logical_monitors, properties)
                // monitor: ((connector, vendor, product, serial), modes, props)
                // mode: (id, width, height, refresh, preferred_scale, supported_scales, props{is-current})
                // logical: (x, y, scale, transform, primary, [(connector, …)], props)
                type Mode = (
                    String,
                    i32,
                    i32,
                    f64,
                    f64,
                    Vec<f64>,
                    HashMap<String, OwnedValue>,
                );
                type Monitor = (
                    (String, String, String, String),
                    Vec<Mode>,
                    HashMap<String, OwnedValue>,
                );
                type Logical = (
                    i32,
                    i32,
                    f64,
                    u32,
                    bool,
                    Vec<(String, String, String, String)>,
                    HashMap<String, OwnedValue>,
                );
                let (_, monitors, logical, _): (
                    u32,
                    Vec<Monitor>,
                    Vec<Logical>,
                    HashMap<String, OwnedValue>,
                ) = reply.body().deserialize().map_err(|e| e.to_string())?;
                let current = |connector: &str| -> Option<(f64, f64)> {
                    let m = monitors.iter().find(|m| m.0 .0 == connector)?;
                    let mode = m.1.iter().find(|md| {
                        md.6.get("is-current")
                            .and_then(|v| bool::try_from(&**v).ok())
                            .unwrap_or(false)
                    })?;
                    Some((mode.1 as f64, mode.2 as f64))
                };
                let mut out: Vec<(bool, Display)> = logical
                    .iter()
                    .filter_map(|l| {
                        let (pw, ph) = current(&l.5.first()?.0)?;
                        // Transforms 1, 3 (and their flipped 5, 7) rotate by 90°.
                        let (pw, ph) = if l.3 % 2 == 1 { (ph, pw) } else { (pw, ph) };
                        let scale = if l.2 > 0.0 { l.2 } else { 1.0 };
                        let d = Display {
                            index: 0,
                            x: l.0 as f64,
                            y: l.1 as f64,
                            width: (pw / scale).round(),
                            height: (ph / scale).round(),
                            scale,
                        };
                        Some((l.4, d))
                    })
                    .collect();
                out.sort_by(|a, b| {
                    let key = |d: &Display| (!(d.x == 0.0 && d.y == 0.0), d.y, d.x);
                    key(&a.1).partial_cmp(&key(&b.1)).unwrap()
                });
                Ok(out
                    .into_iter()
                    .enumerate()
                    .map(|(i, (_, d))| Display { index: i, ..d })
                    .collect())
            }
            .await,
        )
    }

    /// Refresh the cached layout that `sys_info` reports, without opening a session.
    async fn refresh_layout(&self) {
        if let Some(Ok(layout)) = self.mutter_layout().await {
            if !layout.is_empty() {
                *self.last_layout.lock().unwrap() = layout;
            }
        }
    }

    /// On GNOME, give each stream its monitor's scale (matched by logical position); any
    /// failure leaves 1.0, which is right for non-GNOME desktops.
    async fn apply_motion_scales(&self, streams: &mut [Stream]) {
        match self.mutter_layout().await {
            None => {}
            Some(Ok(layout)) => {
                for st in streams.iter_mut() {
                    if let Some(d) = layout.iter().find(|d| d.x == st.x && d.y == st.y) {
                        st.motion_scale = d.scale;
                    }
                }
            }
            Some(Err(e)) => log(&format!(
                "desktop: could not read GNOME monitor scales ({e}); assuming 1.0"
            )),
        }
    }

    async fn close(&self, s: &Session) {
        if let Ok(conn) = self.conn().await {
            if let Ok(p) = Proxy::new(
                conn,
                DEST,
                s.path.as_ref(),
                "org.freedesktop.portal.Session",
            )
            .await
            {
                let _ = p.call_method("Close", &()).await;
            }
        }
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    async fn session(self: &Arc<Self>) -> Result<Session, String> {
        self.touch();
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = self.open_session().await?;
        *guard = Some(s.clone());
        *self.last_layout.lock().unwrap() = displays_of(&s);
        let me = self.clone();
        self.reaper.call_once(move || {
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let idle = me.last_used.lock().unwrap().elapsed() >= IDLE;
                    if idle {
                        if let Some(s) = me.session.lock().await.take() {
                            me.close(&s).await;
                            log("desktop: remote-desktop session closed after idle");
                        }
                    }
                }
            });
        });
        Ok(s)
    }

    async fn drop_session(&self) {
        if let Some(s) = self.session.lock().await.take() {
            self.close(&s).await;
        }
    }

    /// Call a RemoteDesktop Notify* method; if the session died underneath us (closed by
    /// the desktop, revoked by the user), reopen once and retry.
    async fn notify<B, F>(self: &Arc<Self>, method: &'static str, build: F) -> Result<(), String>
    where
        B: serde::Serialize + DynamicType,
        F: Fn(&Session) -> B,
    {
        for attempt in 0..2 {
            let s = self.session().await?;
            let r = self.proxy(RD).await?.call_method(method, &build(&s)).await;
            match r {
                Ok(_) => return Ok(()),
                // Bad arguments are ours to report, not a dead session to reopen.
                Err(e) if attempt == 0 && !e.to_string().contains("Invalid") => {
                    log(&format!(
                        "desktop: {method} failed ({e}); reopening the session"
                    ));
                    self.drop_session().await;
                }
                Err(e) => return Err(format!("{method}: {e}")),
            }
        }
        unreachable!()
    }
}

fn parse_streams(v: Option<&OwnedValue>) -> Vec<Stream> {
    let mut out = Vec::new();
    let Some(Value::Array(arr)) = v.map(|v| &**v) else {
        return out;
    };
    for item in arr.iter() {
        let Value::Structure(st) = item else { continue };
        let f = st.fields();
        let (Some(Value::U32(node)), Some(Value::Dict(props))) = (f.first(), f.get(1)) else {
            continue;
        };
        let pair = |k: &str| -> Option<(f64, f64)> {
            let v: Value = props.get(&k).ok()??;
            let Value::Structure(p) = v else { return None };
            match p.fields() {
                [Value::I32(a), Value::I32(b)] => Some((*a as f64, *b as f64)),
                _ => None,
            }
        };
        let (x, y) = pair("position").unwrap_or((0.0, 0.0));
        let Some((w, h)) = pair("size") else { continue };
        out.push(Stream {
            node: *node,
            x,
            y,
            w,
            h,
            motion_scale: 1.0,
        });
    }
    sort_streams(&mut out);
    out
}

fn sort_streams(s: &mut [Stream]) {
    s.sort_by(|a, b| {
        let origin = |s: &Stream| !(s.x == 0.0 && s.y == 0.0);
        (origin(a), a.y, a.x)
            .partial_cmp(&(origin(b), b.y, b.x))
            .unwrap()
    });
}

fn displays_of(s: &Session) -> Vec<Display> {
    s.streams
        .iter()
        .enumerate()
        .map(|(index, st)| Display {
            index,
            x: st.x,
            y: st.y,
            width: st.w,
            height: st.h,
            scale: st.motion_scale,
        })
        .collect()
}

/// Reach logical point (x, y) on a stream of logical size w×h whose absolute motion is
/// read in `k` pixels per point. xdg-desktop-portal rejects absolute values outside w×h,
/// yet Mutter divides them by k, so with k > 1 only the top-left 1/k of the display is
/// reachable absolutely. Go as far as allowed (strictly inside w×h), then cover the rest with relative motion
/// (logical, unaccelerated for virtual devices). Returns (abs_x, abs_y, rel_dx, rel_dy).
fn split_motion(x: f64, y: f64, w: f64, h: f64, k: f64) -> (f64, f64, f64, f64) {
    // The portal's bound is exclusive: w itself is rejected.
    let ax = (x * k).min(w - 1.0);
    let ay = (y * k).min(h - 1.0);
    let (dx, dy) = (x - ax / k, y - ay / k);
    let snap = |d: f64| if d.abs() < 1e-6 { 0.0 } else { d };
    (ax, ay, snap(dx), snap(dy))
}

/// `file:///home/u/%E5%9C%96%E7%89%87/Screenshot.png` → path.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// X11 keysyms, which the RemoteDesktop portal takes for keyboard input.
fn keysym(k: Key) -> u32 {
    match k {
        Key::Char('\n' | '\r') => 0xff0d,
        Key::Char('\t') => 0xff09,
        Key::Char(c) if (' '..='~').contains(&c) || ('\u{a0}'..='\u{ff}').contains(&c) => c as u32,
        Key::Char(c) => 0x0100_0000 | c as u32,
        Key::Named(n) => match n {
            NamedKey::Return => 0xff0d,
            NamedKey::Tab => 0xff09,
            NamedKey::Space => 0x20,
            NamedKey::Backspace => 0xff08,
            NamedKey::Delete => 0xffff,
            NamedKey::Escape => 0xff1b,
            NamedKey::Insert => 0xff63,
            NamedKey::Home => 0xff50,
            NamedKey::End => 0xff57,
            NamedKey::PageUp => 0xff55,
            NamedKey::PageDown => 0xff56,
            NamedKey::Left => 0xff51,
            NamedKey::Up => 0xff52,
            NamedKey::Right => 0xff53,
            NamedKey::Down => 0xff54,
            NamedKey::F(n) => 0xffbe + (n.clamp(1, 24) as u32 - 1),
            NamedKey::CapsLock => 0xffe5,
            NamedKey::PrintScreen => 0xff61,
            NamedKey::Menu => 0xff67,
            NamedKey::VolumeUp => 0x1008_ff13,
            NamedKey::VolumeDown => 0x1008_ff11,
            NamedKey::Mute => 0x1008_ff12,
            NamedKey::Shift => 0xffe1,
            NamedKey::Ctrl => 0xffe3,
            NamedKey::Alt => 0xffe9,
            NamedKey::Super => 0xffeb,
        },
    }
}

impl Desktop for Portal {
    fn mechanism(&self) -> &'static str {
        "xdg-desktop-portal (Screenshot + RemoteDesktop)"
    }

    fn capture(&self) -> DesktopFuture<'_, Capture> {
        Box::pin(async move {
            // Layout first: it may need the one-time consent, and a screenshot we cannot map
            // to pointer coordinates is not useful for the see→act loop.
            let displays = displays_of(&self.0.session().await?);
            let mut o = Opts::new();
            o.insert("interactive", Value::from(false));
            let r = self
                .0
                .request(SHOT, "Screenshot", o, |o| ("", o), SCREENSHOT_TIMEOUT)
                .await?;
            let uri = r
                .get("uri")
                .and_then(|v| <&str>::try_from(&**v).ok())
                .ok_or("Screenshot returned no uri")?;
            let path =
                file_uri_to_path(uri).ok_or_else(|| format!("unexpected screenshot uri {uri}"))?;
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| format!("read {}: {e}", path.display()));
            // The portal saved it into the user's files; it was only ever ours to read.
            let _ = tokio::fs::remove_file(&path).await;
            // PNG decode of a HiDPI desktop is CPU-heavy: off the async workers.
            let bytes = bytes?;
            let image = tokio::task::spawn_blocking(move || {
                image::load_from_memory(&bytes).map(|i| i.to_rgba8())
            })
            .await
            .map_err(|e| format!("decode worker failed: {e}"))?
            .map_err(|e| format!("decode screenshot: {e}"))?;

            // Place the displays in image space: shift the layout so its bounding box
            // starts at (0,0), and derive the pixel density from the width.
            let min_x = displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
            let min_y = displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min);
            let max_x = displays
                .iter()
                .map(|d| d.x + d.width)
                .fold(f64::NEG_INFINITY, f64::max);
            let pixels_per_point = image.width() as f64 / (max_x - min_x).max(1.0);
            let displays = displays
                .into_iter()
                .map(|d| Display {
                    x: d.x - min_x,
                    y: d.y - min_y,
                    ..d
                })
                .collect();
            Ok(Capture {
                image,
                pixels_per_point,
                displays,
            })
        })
    }

    fn pointer_move(&self, display: usize, x: f64, y: f64) -> DesktopFuture<'_, ()> {
        Box::pin(async move {
            let s = self.0.session().await?;
            let st = s.streams.get(display).ok_or_else(|| {
                format!(
                    "display {display} out of range; {} display(s)",
                    s.streams.len()
                )
            })?;
            if !(0.0..=st.w).contains(&x) || !(0.0..=st.h).contains(&y) {
                return Err(format!(
                    "point ({x}, {y}) outside display {display} bounds {}×{}",
                    st.w, st.h
                ));
            }
            let (node, k) = (st.node, st.motion_scale);
            let (ax, ay, rx, ry) = split_motion(x, y, st.w, st.h, k);
            self.0
                .notify("NotifyPointerMotionAbsolute", move |s| {
                    (s.path.clone(), Opts::new(), node, ax, ay)
                })
                .await?;
            if rx != 0.0 || ry != 0.0 {
                self.0
                    .notify("NotifyPointerMotion", move |s| {
                        (s.path.clone(), Opts::new(), rx, ry)
                    })
                    .await?;
            }
            Ok(())
        })
    }

    fn button(&self, button: Button, down: bool) -> DesktopFuture<'_, ()> {
        let code = match button {
            Button::Left => BTN_LEFT,
            Button::Right => BTN_RIGHT,
        };
        Box::pin(async move {
            self.0
                .notify("NotifyPointerButton", move |s| {
                    (s.path.clone(), Opts::new(), code, down as u32)
                })
                .await
        })
    }

    fn scroll(&self, dx: i32, dy: i32) -> DesktopFuture<'_, ()> {
        Box::pin(async move {
            // Portal: positive steps scroll down/right (content up/left), the opposite of
            // the tool's "positive dy = content up" convention.
            if dy != 0 {
                self.0
                    .notify("NotifyPointerAxisDiscrete", move |s| {
                        (s.path.clone(), Opts::new(), 0u32, -dy)
                    })
                    .await?;
            }
            if dx != 0 {
                self.0
                    .notify("NotifyPointerAxisDiscrete", move |s| {
                        (s.path.clone(), Opts::new(), 1u32, -dx)
                    })
                    .await?;
            }
            Ok(())
        })
    }

    fn key(&self, key: Key, down: bool) -> DesktopFuture<'_, ()> {
        let sym = keysym(key) as i32;
        Box::pin(async move {
            self.0
                .notify("NotifyKeyboardKeysym", move |s| {
                    (s.path.clone(), Opts::new(), sym, down as u32)
                })
                .await
        })
    }

    /// Mutter/KWin only emit keysyms present in the active keymap and drop the rest
    /// silently, so a US layout loses "é" and all CJK. Non-ASCII therefore goes through the
    /// IBus/GTK Unicode entry: ctrl+shift+u, the hex code point, space.
    fn type_char(&self, c: char) -> DesktopFuture<'_, ()> {
        Box::pin(async move {
            let tap = |k: Key| async move {
                self.key(k, true).await?;
                self.key(k, false).await
            };
            if c.is_ascii() {
                return tap(Key::Char(c)).await;
            }
            with_modifiers(self, &[NamedKey::Ctrl, NamedKey::Shift], || {
                tap(Key::Char('u'))
            })
            .await?;
            // The input method must see the entry start (and later the commit) before the
            // next keys; without these pauses every other character comes out as raw hex.
            tokio::time::sleep(UNICODE_ENTRY_PAUSE).await;
            for h in format!("{:x}", c as u32).chars() {
                tap(Key::Char(h)).await?;
            }
            tap(Key::Named(NamedKey::Space)).await?;
            tokio::time::sleep(UNICODE_ENTRY_PAUSE).await;
            Ok(())
        })
    }

    fn facts(&self) -> DesktopFacts {
        let displays = self.0.last_layout.lock().unwrap().clone();
        DesktopFacts {
            consent: self.0.load_token().is_some(),
            displays,
        }
    }

    fn status(&self) -> String {
        let consent = if self.0.load_token().is_some() {
            "remembered"
        } else {
            "not yet given (first use shows a dialog on this screen)"
        };
        let open = self
            .0
            .session
            .try_lock()
            .map(|s| s.is_some())
            .unwrap_or(true);
        format!(
            "{}; consent {consent}; session {}",
            self.mechanism(),
            if open { "open" } else { "closed" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode_percent_escapes() {
        assert_eq!(
            file_uri_to_path("file:///home/david/%E5%9C%96%E7%89%87/Screenshot-1.png").unwrap(),
            PathBuf::from("/home/david/圖片/Screenshot-1.png")
        );
        assert_eq!(
            file_uri_to_path("file:///tmp/a%20b.png").unwrap(),
            PathBuf::from("/tmp/a b.png")
        );
        assert!(file_uri_to_path("https://x/y.png").is_none());
    }

    #[test]
    fn keysyms() {
        assert_eq!(keysym(Key::Char('a')), 0x61);
        assert_eq!(keysym(Key::Char('A')), 0x41);
        assert_eq!(keysym(Key::Char('é')), 0xe9);
        assert_eq!(keysym(Key::Char('中')), 0x0100_4e2d);
        assert_eq!(keysym(Key::Char('\n')), 0xff0d);
        assert_eq!(keysym(Key::Named(NamedKey::F(12))), 0xffc9);
        assert_eq!(keysym(Key::Named(NamedKey::Super)), 0xffeb);
    }

    #[test]
    fn motion_splits_into_absolute_then_relative_when_scaled() {
        // Unscaled: all absolute.
        assert_eq!(
            split_motion(32.0, 770.0, 1728.0, 1080.0, 1.0),
            (32.0, 770.0, 0.0, 0.0)
        );
        // 5/3 scale: x fits (53.3 ≤ 1728), y does not (1283 > 1080) → clamp, then +122 relative.
        let (ax, ay, dx, dy) = split_motion(32.0, 770.0, 1728.0, 1080.0, 5.0 / 3.0);
        assert!((ax - 53.333).abs() < 0.01 && ay == 1079.0 && dx == 0.0);
        assert!((dy - (770.0 - 1079.0 * 0.6)).abs() < 1e-9);
        // Top-left region: purely absolute.
        assert_eq!(
            split_motion(300.0, 300.0, 1728.0, 1080.0, 2.0),
            (600.0, 600.0, 0.0, 0.0)
        );
    }

    #[test]
    fn origin_display_sorts_first() {
        let mk = |node, x, y| Stream {
            node,
            x,
            y,
            w: 10.0,
            h: 10.0,
            motion_scale: 1.0,
        };
        let mut s = vec![mk(1, 1728.0, 0.0), mk(2, -1920.0, 0.0), mk(3, 0.0, 0.0)];
        sort_streams(&mut s);
        assert_eq!(s.iter().map(|s| s.node).collect::<Vec<_>>(), vec![3, 2, 1]);
    }
}
