//! What the bridge is told at startup, and the protocol's fixed timings.

use std::time::Duration;

/// Whether application write requests are carried through to the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Mode {
    /// `N` only. `W` topics are refused and never reach `SetValue`, and the
    /// bridge does not even subscribe to them — the refusal is belt and
    /// braces, because a broker that ignores the subscription set, or a
    /// message already in flight when the mode changed, must still be
    /// refused by the code that acts on it.
    ///
    /// The default: a bridge nobody configured cannot be a control path.
    #[default]
    ReadOnly,
    /// `W` topics become `SetValue` on the uniquely addressed application
    /// item. Exact package enrollment still applies.
    Full,
}

impl Mode {
    /// Whether a write request may proceed to the bus.
    pub fn writes_allowed(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// The protocol's three time constants (`docs/design/bus.md`, MQTT grammar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// How long one keepalive keeps the bridge publishing. Every publication
    /// is gated on this window: no keepalive, or an expired one, and the
    /// bridge is silent.
    pub alive_window: Duration,
    /// The interval between heartbeat publications while alive.
    pub heartbeat: Duration,
    /// The floor between two full republishes.
    ///
    /// This is the rate limit D6 asks for. A keepalive storm renews the alive
    /// window every time — that part is cheap and must not be throttled — but
    /// the full republish it asks for is coalesced: keepalives arriving inside
    /// the floor collapse into a single deferred republish rather than one
    /// per keepalive.
    pub full_publish_min_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            alive_window: Duration::from_secs(60),
            heartbeat: Duration::from_secs(3),
            full_publish_min_interval: Duration::from_secs(5),
        }
    }
}
