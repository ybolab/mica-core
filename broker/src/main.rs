//! `mos-mqtt-broker` — the MQTT broker the device serves locally.
//!
//! This is rumqttd used as a library, not as its own daemon. It reads the
//! three-key file mosd renders into `/run/mos/mqtt-broker.toml`, builds a
//! [`rumqttd::Config`] in code, and blocks in [`rumqttd::Broker::start`].
//!
//! Building the config in code rather than handing rumqttd its own TOML is the
//! point: everything below that is not in the mos-owned file is not a knob.
//! There is no HTTP console, no Prometheus listener, no cluster and no bridge,
//! and none of those can be turned on by editing a file on the device.
//!
//! One listener is served, and it speaks MQTT 3.1.1. That is not a shortcut --
//! rumqttd binds a socket per protocol version, and the settings model carries
//! one port. See `broker_config`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rumqttd::{Broker, ConnectionSettings, RouterConfig, ServerSettings};

mod config;

/// The MQTT broker for mos application item trees and local clients.
#[derive(Debug, Parser)]
#[command(name = "mos-mqtt-broker", version)]
struct Args {
    /// The mos-owned broker configuration, rendered by mosd from the `mqtt`
    /// settings subtree before this unit is started.
    #[arg(long)]
    config: PathBuf,

    /// The credentials file, read only when `auth_enabled` is true. An
    /// argument rather than a constant so tests never read the host's.
    #[arg(long, default_value = "/var/lib/mos/mqtt-broker-users.toml")]
    users: PathBuf,
}

/// How many clients may be connected at once.
///
/// This is an appliance: the bridge, a dashboard or two, and whatever the
/// owner points at it. The router pre-sizes per-connection bookkeeping from
/// this, so it is a memory number as much as a policy one -- a few tens of KiB
/// at 64, against megabytes at rumqttd's demo value of 10010.
const MAX_CONNECTIONS: usize = 64;

/// Outgoing packets the router will hold for one connection before it stops
/// reading more for it. 200 small item-tree publishes is roughly 200 KiB of
/// worst-case backlog per slow client.
const MAX_OUTGOING_PACKET_COUNT: u64 = 200;

/// The commitlog segment size, and how many segments a topic filter keeps.
///
/// Together these bound retained history per filter: 16 KiB x 10 = 160 KiB,
/// and only for filters that actually carry traffic. The item tree publishes
/// small JSON, so 16 KiB is many messages per segment; ten segments is enough
/// history for a client that reconnects, and not enough to matter on a device
/// with 1 GiB of RAM.
const MAX_SEGMENT_SIZE: usize = 16 * 1024;
const MAX_SEGMENT_COUNT: usize = 10;

/// How long a TCP connection has to send its CONNECT before it is dropped.
/// Short, because everything legitimate here is on the same device or the
/// same LAN; the only thing that takes longer is a port scanner holding a slot.
const CONNECTION_TIMEOUT_MS: u16 = 5_000;

/// The largest single publish accepted, in bytes.
///
/// The item tree's payloads are small JSON scalars; 64 KiB is generous for
/// them and still bounds what one client can make the router allocate.
const MAX_PAYLOAD_SIZE: usize = 64 * 1024;

/// Unacknowledged QoS 1/2 messages allowed in flight per connection.
const MAX_INFLIGHT_COUNT: usize = 100;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let args = Args::parse();

    let cfg = config::load(&args.config)?;
    let address = cfg
        .address()
        .with_context(|| format!("in broker config {}", args.config.display()))?;
    let listen = SocketAddr::new(address, cfg.listen_port);

    // Authentication is a plain username -> password map. When it is off, the
    // map is absent entirely rather than empty: an empty map means "auth is on
    // and nobody is enrolled", which refuses every login, and the two must not
    // be spelled the same way.
    let auth = if cfg.auth_enabled {
        let users = config::load_users(&args.users)?;
        if users.is_empty() {
            tracing::warn!(
                path = %args.users.display(),
                "authentication is enabled but no credentials are enrolled; \
                 every connection will be refused until this file lists a user"
            );
        }
        Some(users)
    } else {
        None
    };

    // A log, never a gate. Binding off-host with auth off is a configuration
    // the owner is allowed to choose -- on a trusted segment it is the obvious
    // one -- but it is not a configuration anyone should arrive at by
    // accident, so it is stated once, loudly, at startup.
    if config::is_off_host_unauthenticated(address, cfg.auth_enabled) {
        tracing::warn!(
            %address,
            "the broker is listening off-host with authentication disabled: \
             any host that can reach this address can publish and subscribe \
             without a password"
        );
    }

    tracing::info!(%listen, auth = cfg.auth_enabled, "starting");

    let config = broker_config(listen, auth);

    // `start` owns its own runtime and blocks; there is no async main here and
    // the crate needs no tokio of its own.
    let mut broker = Broker::new(config);
    broker.start().context("the broker exited")?;
    Ok(())
}

/// Build the whole [`rumqttd::Config`] from the two things the mos-owned file
/// decides: where to listen, and who may log in.
///
/// A function rather than a block inside `main` so that the shape of what gets
/// served is a value a test can assert on. A config built inline is a config
/// no test can look at -- two listeners on one port is exactly the defect that
/// hides there.
fn broker_config(listen: SocketAddr, auth: Option<HashMap<String, String>>) -> rumqttd::Config {
    let connections = ConnectionSettings {
        connection_timeout_ms: CONNECTION_TIMEOUT_MS,
        max_payload_size: MAX_PAYLOAD_SIZE,
        max_inflight_count: MAX_INFLIGHT_COUNT,
        auth,
        external_auth: None,
        // A subscriber may create a filter the router has not seen published
        // yet. The item tree is discovered at runtime, so a client that
        // subscribes before mosd has published a service cannot be made to
        // wait for it.
        dynamic_filters: true,
    };

    // Exactly one listener, and it speaks MQTT 3.1.1.
    //
    // rumqttd does not multiplex protocol versions over one socket the way the
    // name "listener" suggests. A `v4` map and a `v5` map are two independent
    // servers, and each one calls `bind()` for itself. Given both the same
    // address and port, one of them loses with `EADDRINUSE` -- and which one
    // loses is a race between two threads, so it differs from boot to boot.
    // rumqttd swallows that into an `error!` and keeps running, so the process
    // stays up serving whichever half won.
    //
    // v4 is the half that has to survive: the only client in the image is
    // mos-mqttd, which connects through rumqttc's top-level `AsyncClient`, and
    // that is the 3.1.1 surface. On a boot where v5 wins the race the bridge
    // cannot reach the broker at all.
    //
    // There is also nowhere to put a second listener: `mqtt.listen` in the
    // settings model carries one address and one port by design.
    //
    // MQTT 5.0 is therefore deferred, not refused. Serving it wants either a
    // second port or a `protocol` key under `mqtt.listen` -- a settings-schema
    // decision, not something to smuggle in here by handing rumqttd a map it
    // will race against itself.
    let v4 = HashMap::from([(
        "mos".to_string(),
        ServerSettings {
            name: "mos".to_string(),
            listen,
            tls: None,
            next_connection_delay_ms: 0,
            connections,
        },
    )]);

    rumqttd::Config {
        id: 0,
        router: RouterConfig {
            max_connections: MAX_CONNECTIONS,
            max_outgoing_packet_count: MAX_OUTGOING_PACKET_COUNT,
            max_segment_size: MAX_SEGMENT_SIZE,
            max_segment_count: MAX_SEGMENT_COUNT,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
        v4: Some(v4),
        // Everything below is deliberately off, and `v5` is the reason this
        // list is worth reading: every one of these is a `bind()` or a thread
        // rumqttd will start without being asked twice. `ws` would be a second
        // wire protocol nobody asked for; `console` and `prometheus` would each
        // open an HTTP listener, and a broker that opened a second network
        // socket on an appliance is a second thing to attack and a second
        // thing to explain. `cluster` and `bridge` have no meaning on a single
        // device -- the bridge in this image is mos-mqttd, which is a client.
        v5: None,
        ws: None,
        cluster: None,
        console: None,
        bridge: None,
        prometheus: None,
        metrics: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    /// How long the connect test will wait for the broker to bind, and then
    /// for its CONNACK. Both are sub-second in practice; this is slack for a
    /// loaded CI box, not an expected duration.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    /// Exactly one listener, asserted on the VALUE.
    ///
    /// rumqttd runs `v4` and `v5` as separate servers that each `bind()` for
    /// themselves, so configuring both on one address means one of them dies
    /// with `EADDRINUSE` and which one dies is a thread race. Nothing here
    /// binds anything: the whole point is that the mistake is visible in the
    /// value, before a socket is involved.
    ///
    /// It asserts the entire listener surface rather than just `v5`, because
    /// `ws` is the same mistake spelled differently, and `console` and
    /// `prometheus` each open a socket of their own.
    #[test]
    fn exactly_one_listener_is_configured() {
        let listen = loopback(1883);
        let config = broker_config(listen, None);

        let v4 = config
            .v4
            .as_ref()
            .expect("v4 is the listener mos-mqttd speaks, and it must be served");
        assert_eq!(
            v4.len(),
            1,
            "one address and one port can carry one listener; got {:?}",
            v4.keys().collect::<Vec<_>>()
        );
        let settings = v4
            .get("mos")
            .expect("the single listener is named `mos`; the name is what rumqttd logs");
        assert_eq!(settings.name, "mos");
        assert_eq!(
            settings.listen, listen,
            "the listener binds what mosd rendered"
        );

        // Each of these is a second `bind()` or a second thread if it is ever
        // set. `v5` is the one that was actually set and actually broke.
        assert!(
            config.v5.is_none(),
            "a v5 map is a second server binding the same port; \
             MQTT 5.0 needs a port or a settings key of its own first"
        );
        assert!(config.ws.is_none(), "websockets would be a second listener");
        assert!(config.cluster.is_none(), "there is one device");
        assert!(config.console.is_none(), "the console is an HTTP listener");
        assert!(
            config.bridge.is_none(),
            "the bridge in this image is a client"
        );
        assert!(
            config.prometheus.is_none(),
            "prometheus is an HTTP listener"
        );
        assert!(
            config.metrics.is_none(),
            "metrics would start a timer thread"
        );
    }

    /// The credentials map reaches the connection settings, and its absence is
    /// spelled `None` rather than an empty map -- an empty map is "auth is on
    /// and nobody is enrolled", which refuses every login.
    #[test]
    fn auth_reaches_the_listener_and_off_is_not_an_empty_map() {
        let settings = |auth| {
            broker_config(loopback(1883), auth)
                .v4
                .expect("v4 is always served")
                .remove("mos")
                .expect("the single listener is named `mos`")
                .connections
        };

        assert!(settings(None).auth.is_none(), "auth off is no map at all");

        let users = HashMap::from([("venus".to_string(), "hunter2".to_string())]);
        assert_eq!(settings(Some(users.clone())).auth, Some(users));
    }

    /// An ephemeral port, taken from the kernel and handed straight back.
    ///
    /// There is a window between the drop and rumqttd's own `bind` in which
    /// another process could claim the port. Linux draws ephemeral ports from
    /// a range tens of thousands wide and does not hand the same one out
    /// twice in a row, so that window is microseconds against a very large
    /// space. The alternative -- a port fixed in the source -- collides with a
    /// developer's own broker, or with a second copy of this test, far more
    /// often than that.
    fn free_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("binding an ephemeral loopback port")
            .local_addr()
            .expect("a bound listener has a local address")
            .port()
    }

    /// Block until something accepts on `listen`.
    ///
    /// This is the assertion the old test suite never made. rumqttd reports a
    /// failed `bind` as a log line and keeps running, so the only way to know
    /// the listener is real is to connect to it.
    fn wait_until_listening(listen: SocketAddr) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&listen, Duration::from_millis(200)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("nothing bound {listen} within {TEST_TIMEOUT:?}");
    }

    /// End to end: start the broker the way `main` does and connect to it with
    /// the client mos-mqttd uses.
    ///
    /// `rumqttc::Client` is the MQTT 3.1.1 surface -- rumqttc's 5.0 support
    /// lives in a separate `rumqttc::v5` module that the bridge does not use.
    /// So this test fails on precisely the boots the bridge would have failed
    /// on, which is the property the config-only test cannot have.
    #[test]
    fn a_v4_client_connects_to_the_started_broker() {
        let listen = loopback(free_port());

        // `start` blocks and rumqttd offers no shutdown, so this thread is
        // deliberately never joined. nextest runs each test in its own
        // process, so it ends when the process does.
        std::thread::spawn(move || {
            let mut broker = Broker::new(broker_config(listen, None));
            if let Err(err) = broker.start() {
                eprintln!("the broker exited: {err}");
            }
        });
        wait_until_listening(listen);

        let mut options =
            rumqttc::MqttOptions::new("mos-broker-test", listen.ip().to_string(), listen.port());
        options.set_keep_alive(Duration::from_secs(5));
        // Bound to a name, not to `_`: dropping the client closes the request
        // channel and ends the event loop before it can see a CONNACK.
        let (_client, mut connection) = rumqttc::Client::new(options, 10);

        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "no CONNACK from {listen} within {TEST_TIMEOUT:?}"
            );
            match connection.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(ack)))) => {
                    assert_eq!(
                        ack.code,
                        rumqttc::ConnectReturnCode::Success,
                        "the broker refused an unauthenticated connect with auth off"
                    );
                    return;
                }
                // Outgoing CONNECT, and anything else on the way to the ack.
                Ok(Ok(_)) => continue,
                Ok(Err(err)) => panic!("connecting to {listen}: {err}"),
                Err(rumqttc::RecvTimeoutError::Timeout) => continue,
                Err(err) => panic!("the client's event loop ended early: {err:?}"),
            }
        }
    }
    #[test]
    fn the_bridge_connects_with_its_private_credentials_when_auth_is_enabled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials.json");
        std::fs::write(
            &credentials,
            r#"{"username":"bridge","password":"test-only-password"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listen = loopback(free_port());

        // `start` blocks and rumqttd offers no shutdown, so this thread is
        // deliberately never joined. nextest runs each test in its own
        // process, so it ends when the process does.
        std::thread::spawn(move || {
            let mut broker = Broker::new(broker_config(
                listen,
                Some(HashMap::from([(
                    "bridge".to_string(),
                    "test-only-password".to_string(),
                )])),
            ));
            if let Err(err) = broker.start() {
                eprintln!("the broker exited: {err}");
            }
        });
        wait_until_listening(listen);

        let options = mos_mqttd::runtime::mqtt_options(&mos_mqttd::runtime::Settings {
            device_id: "test-device".into(),
            applications_dir: dir.path().join("applications"),
            broker_host: listen.ip().to_string(),
            broker_port: listen.port(),
            client_id: "authenticated-bridge-test".into(),
            credentials_file: credentials,
            mode: mos_mqttd::config::Mode::ReadOnly,
            session_bus: true,
            timings: mos_mqttd::config::Timings::default(),
        })
        .unwrap();
        // Bound to a name, not to `_`: dropping the client closes the request
        // channel and ends the event loop before it can see a CONNACK.
        let (_client, mut connection) = rumqttc::Client::new(options, 10);

        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "no CONNACK from {listen} within {TEST_TIMEOUT:?}"
            );
            match connection.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(ack)))) => {
                    assert_eq!(
                        ack.code,
                        rumqttc::ConnectReturnCode::Success,
                        "the broker refused the bridge credentials"
                    );
                    return;
                }
                // Outgoing CONNECT, and anything else on the way to the ack.
                Ok(Ok(_)) => continue,
                Ok(Err(err)) => panic!("connecting to {listen}: {err}"),
                Err(rumqttc::RecvTimeoutError::Timeout) => continue,
                Err(err) => panic!("the client's event loop ended early: {err:?}"),
            }
        }
    }
}
