//! `mos-mqttd` — the MQTT application-data bridge.
//!
//! The bridge discovers only exact `com.mos.*` names installed in its
//! package-owned enrollment directory and knows their `GetItems`,
//! `ItemsChanged` and `SetValue` application surface. It never calls or
//! subscribes to mosd. Device identity arrives as runtime configuration, so no
//! system setting, state, signal, method or action becomes an MQTT item.
//!
//! # The protocol is the mos-native grammar, and only that
//!
//! `N|R|W/<deviceId>/<class>/<instance>/<path>` with `{"value": ...}`
//! payloads, a keepalive-triggered rate-limited full republish terminated by
//! `full_publish_completed`, a 3 s heartbeat, and read-only vs full modes.
//! Sparkplug B is **not** implemented here: `docs/design/bus.md` records
//! that evaluation, and its outcome is that a Sparkplug publisher would sit
//! *beside* this one rather than replace it. Nothing in this crate is
//! generalised in anticipation of it.
//!
//! # Shape: a state machine, a transport, and a source
//!
//! [`bridge::Bridge`] is the whole protocol, and it is a pure state machine:
//! events in ([`bridge::Bridge::on_keepalive`], [`bridge::Bridge::on_items_changed`],
//! [`bridge::Bridge::on_request`], [`bridge::Bridge::on_tick`],
//! [`bridge::Bridge::on_service_vanished`]), [`bridge::Effects`] out. It owns
//! no clock, no socket and no bus handle — time arrives as an explicit
//! monotonic `now`, so every protocol decision is reproducible.
//!
//! Around it sit two traits, [`transport::Transport`] (the broker) and
//! [`source::ItemSource`] (application buses), joined by [`runtime::apply`].
//!
//! The protocol tests drive the **production** path — the same
//! [`bridge::Bridge`], the same payload encoder and masker, the same
//! [`runtime::apply`] — against an in-memory [`transport::Transport`] rather
//! than a real MQTT broker. The abstraction boundary is deliberately drawn
//! *below* everything this crate is responsible for: topic grammar, payload
//! shape, masking, liveness gating, rate limiting and mode enforcement are all
//! above it and all covered. A real broker in CI would instead test rumqttc's
//! TCP client, and would skip — reporting green while asserting nothing — on
//! every machine without one. What the double does not cover is the rumqttc
//! wiring in [`runtime::run`], which is kept correspondingly thin.
//!
//! # Application write results do not travel
//!
//! A `W` becomes `SetValue` on the uniquely addressed application and stops
//! there. The grammar has no acknowledgement topic. A subscriber observes a
//! successful write through the application's later `ItemsChanged`; the
//! bridge publishes no synthetic system result or control acknowledgement.
//!
//! # Secrets
//!
//! Applications own source-side secret redaction. The publish-side masking in
//! [`payload`] strips known secret-shaped paths and keys from every payload as
//! an independent last line of defence.

pub mod bridge;
pub mod config;
pub mod enrollment;
pub mod item;
pub mod payload;
pub mod runtime;
pub mod source;
pub mod topic;
pub mod transport;
