//! Platform backends behind the `Desktop` trait. `desktop()` picks the one for this build.

pub mod desktop;
pub mod linux;

use desktop::Desktop;

static WLROOTS: linux::wlroots::Wlroots = linux::wlroots::Wlroots;

/// The desktop backend for this node. wlroots only today; selection by probing (portal vs
/// wlroots) belongs here when a second backend exists.
pub fn desktop() -> &'static dyn Desktop {
    &WLROOTS
}
