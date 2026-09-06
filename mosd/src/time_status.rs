//! Read-only observation of timesyncd's synchronization state.
//!
//! The shape [`crate::network_state`] takes: a trait with an unavailable
//! default so a dry-run daemon or a test can never inspect its host, and a
//! production implementation only `main.rs` attaches. The observation is
//! served by the `GetTimeStatus` bus method and surfaced read-only through
//! apid; nothing here writes anything, and nothing here can stop timesyncd's
//! retries — the classification REPORTS a degraded state, it never acts on
//! one.
//!
//! Evidence comes from two well-known services: `org.freedesktop.timesync1`
//! (the selected server and the last NTP reply) and
//! `org.freedesktop.timedate1` (`NTPSynchronized`). Every read is soft — a
//! property that does not answer becomes absent evidence, never an error —
//! because a status surface that fails when the thing it reports on is down
//! has inverted its own job.
//!
//! **What `NTPSynchronized` is, because the reported state rests on it.**
//! timedated computes it as `adjtimex().maxerror < 16 s` — the kernel's own
//! bound on how wrong the clock may be — and NOT as "an NTP reply arrived".
//! The two are different claims and they can disagree for a long time:
//! timesyncd writes `maxerror` back down only on a sample it accepts, so a
//! spike-rejected reply leaves that bound growing at the kernel's tolerance
//! while usable-looking replies keep arriving. The state names here are
//! chosen to survive that: none of them promises the device is on its way to
//! being synchronized (RFCT-299).
//!
//! **A signal that did not answer is not a signal that answered no.** Absent
//! evidence stays absent all the way to the wire: it never becomes a state
//! that asserts something about the clock, and nothing else — a sample, a
//! stratum, a previous reading — is substituted to fill the gap. Where the
//! `NTPSynchronized` read is what went missing, the state is `unknown` and
//! the payload's `synchronized` member is absent, and that pair is what tells
//! a reader which of the two happened (RFCT-300).

use anyhow::Result;
use serde_json::{Value as Json, json};
use zbus::zvariant::Value;

/// One classified synchronization state, the four PLAN-044 names.
///
/// `Unknown` is deliberately a fifth, and it is the state for a signal that
/// could not be READ rather than for a particular daemon being down: it is
/// what the surface reports when the evidence a state rests on never
/// arrived. Reporting one of the four there would claim evidence nobody has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// The kernel reports a bounded clock error: timedate1's
    /// `NTPSynchronized`, which is `adjtimex().maxerror < 16 s`.
    Synchronized,
    /// timesyncd has a server and is exchanging packets with it, and the
    /// kernel does not report a bounded clock error.
    ///
    /// Named for what is observed rather than for a destination. This is NOT
    /// "nearly synchronized": a device can hold it indefinitely with usable
    /// replies in hand — every reply rejected as a spike reaches no
    /// `clock_adjtime` call at all — so a word promising convergence would be
    /// a claim the evidence does not carry. `synchronized` in the payload is
    /// the bit this rests on, and `sample` is what the server last said.
    Polling,
    /// timesyncd is running and has no usable server — no network, no
    /// resolvable name — and keeps retrying on the pinned 30-second policy.
    OfflineDegraded,
    /// A server answered and its replies cannot be used: an unsynchronized
    /// leap indicator or an out-of-range stratum.
    InvalidSource,
    /// A signal the reported state would rest on could not be read:
    /// timesyncd is not observable on the bus at all, or timedate1 did not
    /// answer `NTPSynchronized`.
    ///
    /// Not a claim about the clock, and deliberately not one — a device
    /// nobody could query is not a device that was queried and found out of
    /// sync (RFCT-300). The payload says which read went missing in `detail`,
    /// and the raw `synchronized` member stays absent beside it, so a reader
    /// can tell "read and false" from "not read".
    Unknown,
}

impl SyncStatus {
    /// The wire spelling the API and UI consume.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synchronized => "synchronized",
            Self::Polling => "polling",
            Self::OfflineDegraded => "offline-degraded",
            Self::InvalidSource => "invalid-source",
            Self::Unknown => "unknown",
        }
    }
}

/// The last NTP reply, as far as timesyncd's `NTPMessage` property tells it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NtpSample {
    /// Leap indicator; 3 is "clock not synchronized" and marks the source
    /// unusable.
    pub leap: u32,
    /// Server stratum; 0 (kiss-of-death) and 16+ (unsynchronized) mark the
    /// source unusable.
    pub stratum: u32,
    /// Whether timesyncd flagged the sample as a spike and discarded it.
    pub spike: bool,
    /// The sample's clock offset in seconds, from the four NTP timestamps.
    pub offset_seconds: f64,
    /// How many replies this server has given over the current connection.
    pub packet_count: u64,
}

/// Everything the classifier reads. Each field is what one soft bus read
/// produced; absent means the read did not answer.
#[derive(Debug, Clone, Default)]
pub struct TimesyncEvidence {
    /// Whether `org.freedesktop.timesync1` answered at all.
    pub service_reachable: bool,
    /// timedate1's `NTPSynchronized`: the kernel's own synchronized bit.
    pub ntp_synchronized: Option<bool>,
    /// The server timesyncd currently talks to, by configured name.
    pub server_name: Option<String>,
    /// The same server's resolved address, formatted.
    pub server_address: Option<String>,
    /// The last reply, when the server has answered at least once.
    pub sample: Option<NtpSample>,
}

/// timesyncd's own step/slew boundary (`NTP_MAX_ADJUST`, 0.4 s): an offset
/// beyond it is corrected by stepping the clock, within it by slewing.
///
/// Transcribed from systemd rather than read from anywhere, the same
/// discipline `verify/src/checks-time.ts` applies to the pinned intervals:
/// this is the number the audit distinction rests on, and an image
/// disagreeing with it is the failure whichever side moved.
const STEP_THRESHOLD_SECONDS: f64 = 0.4;

/// Classify `evidence` into the one status the surface reports.
///
/// Pure and deterministic — the whole of the status decision, tested without
/// a bus. Order is meaning: a source that answered garbage outranks
/// "polling" (asking it harder will not help), and the kernel's
/// synchronized bit outranks everything else that is still true, because a
/// disciplined clock with a currently-unreachable server is a device whose
/// time is RIGHT and whose retries continue on the pinned policy.
#[must_use]
pub fn classify(evidence: &TimesyncEvidence) -> SyncStatus {
    if !evidence.service_reachable {
        return SyncStatus::Unknown;
    }
    if let Some(sample) = &evidence.sample
        && (sample.leap == 3 || sample.stratum == 0 || sample.stratum >= 16)
    {
        return SyncStatus::InvalidSource;
    }
    // Every state below rests on timedate1's bit: `synchronized` asserts it,
    // `polling` and `offline-degraded` assert its absence, because both rank
    // under `synchronized` and are only reached once it is ruled out. A read
    // that did not answer is not a read that answered `false`, so none of the
    // three may be reported off it (RFCT-300). `invalid-source` above is not
    // affected: it rests on the sample alone, already outranks the bit, and
    // claims nothing about the clock -- only that the source's replies are
    // unusable, which WAS read.
    let Some(synchronized) = evidence.ntp_synchronized else {
        return SyncStatus::Unknown;
    };
    if synchronized {
        return SyncStatus::Synchronized;
    }
    if evidence.server_name.is_some() || evidence.server_address.is_some() {
        return SyncStatus::Polling;
    }
    SyncStatus::OfflineDegraded
}

/// Which unread signal left the state `unknown`.
///
/// The state is one word for "a signal this rests on did not answer", and the
/// two signals live in different services, so the detail names the one that
/// went missing: sending an operator to a daemon that answered fine is the
/// failure a single fixed sentence would cause.
fn unknown_detail(evidence: &TimesyncEvidence) -> &'static str {
    if evidence.service_reachable {
        "systemd-timedated did not answer NTPSynchronized: the kernel's bound on the clock error was not read"
    } else {
        "systemd-timesyncd is not reachable on the bus"
    }
}

/// `evidence` rendered as the JSON the bus method serves.
///
/// The `correction` member is the step-versus-drift distinction PLAN-044
/// requires *where the evidence allows*: present only when a sample carries an
/// offset, `"step"` past timesyncd's own [`STEP_THRESHOLD_SECONDS`] and
/// `"slew"` inside it. Absent evidence stays absent — no member is
/// manufactured for a question the device cannot answer.
#[must_use]
pub fn status_json(evidence: &TimesyncEvidence) -> Json {
    let status = classify(evidence);
    let mut root = serde_json::Map::new();
    root.insert("status".to_string(), json!(status.as_str()));
    if let Some(synchronized) = evidence.ntp_synchronized {
        root.insert("synchronized".to_string(), json!(synchronized));
    }
    if evidence.server_name.is_some() || evidence.server_address.is_some() {
        root.insert(
            "server".to_string(),
            json!({
                "name": evidence.server_name,
                "address": evidence.server_address,
            }),
        );
    }
    if let Some(sample) = &evidence.sample {
        let correction = if sample.offset_seconds.abs() > STEP_THRESHOLD_SECONDS {
            "step"
        } else {
            "slew"
        };
        root.insert(
            "sample".to_string(),
            json!({
                "leap": sample.leap,
                "stratum": sample.stratum,
                "spike": sample.spike,
                "offsetSeconds": sample.offset_seconds,
                "packetCount": sample.packet_count,
                "correction": correction,
            }),
        );
    }
    if status == SyncStatus::Unknown {
        root.insert("detail".to_string(), json!(unknown_detail(evidence)));
    }
    Json::Object(root)
}

/// Read-only source of time-synchronization evidence.
#[async_trait::async_trait]
pub trait TimeStatusSource: Send + Sync {
    /// Observe the current evidence.
    ///
    /// # Errors
    ///
    /// Returns an error only when this daemon has no observer at all; a
    /// production observer answers with absent evidence instead.
    async fn observe(&self) -> Result<TimesyncEvidence>;
}

/// The observer a daemon without host access has: none.
pub struct UnavailableTimeStatus;

#[async_trait::async_trait]
impl TimeStatusSource for UnavailableTimeStatus {
    async fn observe(&self) -> Result<TimesyncEvidence> {
        Err(anyhow::anyhow!(
            "this daemon observes no time synchronization"
        ))
    }
}

/// Production observer over the system bus.
///
/// The connection is made lazily inside every call, the hostname executor's
/// shape: constructing this never touches the host, and a bus that comes and
/// goes costs a reconnect, not a wedged cache.
pub struct SystemdTimesync;

/// Field indexes of timesync1's `NTPMessage` structure
/// (`(uuuuittayttttbtt)`): leap, version, mode, stratum, precision,
/// root delay, root dispersion, reference id, and then the four NTP
/// timestamps (origin, receive, transmit, destination, in microseconds),
/// the spike flag, the packet count and the jitter.
const FIELD_LEAP: usize = 0;
const FIELD_STRATUM: usize = 3;
const FIELD_ORIGIN: usize = 8;
const FIELD_RECEIVE: usize = 9;
const FIELD_TRANSMIT: usize = 10;
const FIELD_DESTINATION: usize = 11;
const FIELD_SPIKE: usize = 12;
const FIELD_PACKET_COUNT: usize = 13;

fn field_u32(fields: &[Value<'_>], index: usize) -> Option<u32> {
    match fields.get(index)? {
        Value::U32(value) => Some(*value),
        _ => None,
    }
}

fn field_u64(fields: &[Value<'_>], index: usize) -> Option<u64> {
    match fields.get(index)? {
        Value::U64(value) => Some(*value),
        _ => None,
    }
}

fn field_bool(fields: &[Value<'_>], index: usize) -> Option<bool> {
    match fields.get(index)? {
        Value::Bool(value) => Some(*value),
        _ => None,
    }
}

/// Decode one `NTPMessage` structure's fields into an [`NtpSample`].
///
/// Over the field slice rather than a typed tuple, so a systemd that grows
/// the structure keeps decoding (the leading fields are ABI) and a test can
/// hand in a literal `Vec<Value>` with no bus anywhere. `None` when the shape
/// is not the documented one — absent evidence, never an error.
///
/// The offset is the standard NTP calculation over the four timestamps,
/// `((receive - origin) + (transmit - destination)) / 2`, done in `i128` so a
/// wildly wrong clock cannot overflow it.
pub fn parse_ntp_message(fields: &[Value<'_>]) -> Option<NtpSample> {
    let origin = i128::from(field_u64(fields, FIELD_ORIGIN)?);
    let receive = i128::from(field_u64(fields, FIELD_RECEIVE)?);
    let transmit = i128::from(field_u64(fields, FIELD_TRANSMIT)?);
    let destination = i128::from(field_u64(fields, FIELD_DESTINATION)?);
    let offset_usec = ((receive - origin) + (transmit - destination)) / 2;
    Some(NtpSample {
        leap: field_u32(fields, FIELD_LEAP)?,
        stratum: field_u32(fields, FIELD_STRATUM)?,
        spike: field_bool(fields, FIELD_SPIKE)?,
        offset_seconds: offset_usec as f64 / 1_000_000.0,
        packet_count: field_u64(fields, FIELD_PACKET_COUNT)?,
    })
}

/// Format timesync1's `ServerAddress` — an address family and raw bytes —
/// the way `timedatectl` would print it.
fn format_server_address(family: i32, bytes: &[u8]) -> Option<String> {
    match (family, bytes.len()) {
        // AF_INET
        (2, 4) => Some(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()),
        // AF_INET6
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(std::net::Ipv6Addr::from(octets).to_string())
        }
        _ => None,
    }
}

#[async_trait::async_trait]
impl TimeStatusSource for SystemdTimesync {
    async fn observe(&self) -> Result<TimesyncEvidence> {
        let connection = zbus::Connection::system().await?;
        let mut evidence = TimesyncEvidence::default();

        if let Ok(timesync) = zbus::Proxy::new(
            &connection,
            "org.freedesktop.timesync1",
            "/org/freedesktop/timesync1",
            "org.freedesktop.timesync1.Manager",
        )
        .await
        {
            if let Ok(name) = timesync.get_property::<String>("ServerName").await {
                evidence.service_reachable = true;
                if !name.is_empty() {
                    evidence.server_name = Some(name);
                }
            }
            if let Ok((family, bytes)) = timesync
                .get_property::<(i32, Vec<u8>)>("ServerAddress")
                .await
            {
                evidence.service_reachable = true;
                evidence.server_address = format_server_address(family, &bytes);
            }
            if let Ok(message) = timesync
                .get_property::<zbus::zvariant::OwnedValue>("NTPMessage")
                .await
            {
                evidence.service_reachable = true;
                if let Value::Structure(structure) = &Value::from(message) {
                    evidence.sample = parse_ntp_message(structure.fields());
                }
            }
        }

        if let Ok(timedate) = zbus::Proxy::new(
            &connection,
            "org.freedesktop.timedate1",
            "/org/freedesktop/timedate1",
            "org.freedesktop.timedate1",
        )
        .await
            && let Ok(synchronized) = timedate.get_property::<bool>("NTPSynchronized").await
        {
            evidence.ntp_synchronized = Some(synchronized);
        }

        Ok(evidence)
    }
}

/// Where the trusted-clock floor is persisted: timesyncd's saved clock file,
/// whose MTIME is the value (`docs/design/time.md` §3.2).
///
/// `/var/lib/systemd/timesync` is a bind mount whose source is
/// `/mnt/state/timesync`, so this path survives a reboot and an A/B update.
pub const SAVED_CLOCK_PATH: &str = "/var/lib/systemd/timesync/clock";

/// Where the kernel reports how long this boot has been running.
const UPTIME_PATH: &str = "/proc/uptime";

/// The two signals PLAN-071 §7's predicate reads, gathered once.
///
/// Deliberately the two `docs/design/time.md` §3 already defines rather than
/// a third notion of trusted time: the classified synchronization state
/// (§5's `synchronized`, which is the kernel's own bound on the clock error)
/// and whether the saved floor has moved forward during this boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockTrust {
    /// The classified state, or `None` when this daemon has no observer at
    /// all (dry-run) — which is not evidence of anything and is treated as
    /// unread, exactly as [`SyncStatus::Unknown`] is.
    pub status: Option<SyncStatus>,
    /// The saved floor's mtime is later than the instant this boot started:
    /// timesyncd is running and writing the floor forward.
    pub floor_advanced: bool,
}

impl ClockTrust {
    /// Whether the device believes its clock, in PLAN-071 §7's exact terms:
    /// *timesyncd reporting `synchronized`, or a floor that has advanced
    /// since boot.*
    #[must_use]
    pub fn believed(&self) -> bool {
        self.status == Some(SyncStatus::Synchronized) || self.floor_advanced
    }

    /// Why the clock may not be believed, or `None` when it may.
    ///
    /// The sentence names BOTH limbs as they were observed, because the
    /// operator reading a `clock-untrusted` deferral has to know which one
    /// to go and fix — a device with no network and a live floor is a
    /// different problem from one whose STATE bind never arrived.
    #[must_use]
    pub fn untrusted_reason(&self) -> Option<String> {
        if self.believed() {
            return None;
        }
        Some(format!(
            "the clock is not one this device believes: time synchronization reports `{}` \
             and the saved floor at {SAVED_CLOCK_PATH} has not advanced since boot. \
             A maintenance window is UTC wall-clock, so an automatic install is deferred \
             until one of the two holds; checks and fetches are unaffected",
            self.status.map_or("unknown", SyncStatus::as_str),
        ))
    }
}

/// Whether the saved floor has moved forward during this boot.
///
/// **What this asserts, and what it does not.** timesyncd touches the saved
/// clock file every `SaveIntervalSec` and advances a clock that is behind it
/// at startup, so an mtime later than the boot instant says the floor
/// MECHANISM is alive: STATE is bound, timesyncd is running, and the device's
/// notion of now is being persisted forward. It does NOT assert the absolute
/// value is right — nothing offline can. That is what PLAN-071 §7 asks for
/// (`offline-degraded` *with no advance* is the deferral case), and the
/// stronger claim is [`SyncStatus::Synchronized`]'s, which is the other limb.
///
/// `false` on an unreadable file or an unreadable uptime: an unread signal is
/// not an advance, and the closed side here is deferring the install.
#[must_use]
pub fn saved_floor_advanced(clock_path: &std::path::Path, uptime_path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(clock_path) else {
        return false;
    };
    let Ok(saved) = metadata.modified() else {
        return false;
    };
    let Some(uptime) = read_uptime(uptime_path) else {
        return false;
    };
    let Some(boot) = std::time::SystemTime::now().checked_sub(uptime) else {
        return false;
    };
    saved > boot
}

/// This boot's age, from the first field of `/proc/uptime`.
fn read_uptime(path: &std::path::Path) -> Option<std::time::Duration> {
    let contents = std::fs::read_to_string(path).ok()?;
    let seconds: f64 = contents.split_whitespace().next()?.parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| std::time::Duration::from_secs_f64(seconds))
}

/// The production reading of both limbs, from the host's own files.
#[must_use]
pub fn observed_clock_trust(status: Option<SyncStatus>) -> ClockTrust {
    ClockTrust {
        status,
        floor_advanced: saved_floor_advanced(
            std::path::Path::new(SAVED_CLOCK_PATH),
            std::path::Path::new(UPTIME_PATH),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(leap: u32, stratum: u32, offset_seconds: f64) -> NtpSample {
        NtpSample {
            leap,
            stratum,
            spike: false,
            offset_seconds,
            packet_count: 1,
        }
    }

    /// The online path: a reachable daemon, a selected server, and the
    /// kernel's synchronized bit — the state a healthy networked device
    /// settles into.
    #[test]
    fn a_disciplined_clock_classifies_synchronized() {
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(true),
            server_name: Some("0.pool.ntp.org".to_string()),
            server_address: Some("192.0.2.7".to_string()),
            sample: Some(sample(0, 2, 0.012)),
        };
        assert_eq!(classify(&evidence), SyncStatus::Synchronized);
    }

    /// A server is selected and packets are being exchanged, but the kernel
    /// bit is not set: polling, not degraded.
    #[test]
    fn a_selected_server_without_the_kernel_bit_is_polling() {
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(false),
            server_name: Some("0.pool.ntp.org".to_string()),
            ..TimesyncEvidence::default()
        };
        assert_eq!(classify(&evidence), SyncStatus::Polling);
    }

    /// RFCT-299, the bench state: timesyncd reachable, a server selected, a
    /// usable reply in hand, and timedate1's bit clear.
    ///
    /// The report has to be a word that does not promise the clock is about
    /// to be right. `NTPSynchronized` is `adjtimex().maxerror < 16 s`, not
    /// "a reply arrived", and this combination is not necessarily a moment on
    /// the way to anything: a reply timesyncd rejects as a spike never
    /// reaches `clock_adjtime`, so a device can hold usable samples here for
    /// as long as that lasts. The evidence each half rests on stays in the
    /// payload beside the state.
    #[test]
    fn a_usable_sample_without_the_kernel_bit_does_not_claim_convergence() {
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(false),
            server_name: Some("0.pool.ntp.org".to_string()),
            server_address: Some("192.0.2.7".to_string()),
            sample: Some(sample(0, 2, 0.004)),
        };
        let value = status_json(&evidence);
        assert_eq!(value["status"], "polling");
        assert_eq!(value["synchronized"], json!(false));
        assert_eq!(value["sample"]["stratum"], 2);
        assert_eq!(value["server"]["name"], "0.pool.ntp.org");
    }

    /// The offline path: the daemon runs, nothing resolved, no reply ever.
    /// The classification is degraded — and it is ONLY a classification;
    /// retries are timesyncd's pinned 30-second policy and nothing in this
    /// module has a handle with which to stop them.
    #[test]
    fn no_usable_server_is_offline_degraded() {
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(false),
            ..TimesyncEvidence::default()
        };
        assert_eq!(classify(&evidence), SyncStatus::OfflineDegraded);
    }

    /// A source that answers garbage outranks "polling": leap 3 and the
    /// out-of-range strata each mark the reply unusable on its own.
    #[test]
    fn an_unusable_reply_is_invalid_source() {
        for bad in [sample(3, 2, 0.0), sample(0, 0, 0.0), sample(0, 16, 0.0)] {
            let evidence = TimesyncEvidence {
                service_reachable: true,
                ntp_synchronized: Some(false),
                server_name: Some("bad.example".to_string()),
                sample: Some(bad),
                ..TimesyncEvidence::default()
            };
            assert_eq!(classify(&evidence), SyncStatus::InvalidSource, "{bad:?}");
        }
        // And a healthy reply is not: the positive control. The kernel bit
        // is stated rather than left at its default, because the state this
        // control names is one of the three that rest on it having been read
        // (RFCT-300) -- omitting it would make the control assert `unknown`
        // for a reason that has nothing to do with the sample.
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(false),
            server_name: Some("good.example".to_string()),
            sample: Some(sample(0, 2, 0.0)),
            ..TimesyncEvidence::default()
        };
        assert_eq!(classify(&evidence), SyncStatus::Polling);
    }

    /// No observer contact at all is `unknown`, not `offline-degraded`:
    /// claiming network evidence nobody has would send the operator to the
    /// wrong cable.
    #[test]
    fn an_unreachable_daemon_is_unknown() {
        assert_eq!(classify(&TimesyncEvidence::default()), SyncStatus::Unknown);
    }

    /// RFCT-300: timedate1's property read did not answer. `None` and
    /// `Some(false)` are different facts — one is a device nobody could
    /// query, the other a device that was queried and is not synchronized —
    /// and every state below `invalid-source` rests on that bit: the first
    /// asserts it, the other two assert its absence. So an unread bit is
    /// `unknown`, and the states that would claim something about the clock
    /// are unreachable without it.
    ///
    /// `invalid-source` is NOT: it rests on the sample alone, ranks above the
    /// bit already, and says nothing about the clock — only that the source's
    /// replies are unusable, which was read.
    #[test]
    fn an_unread_kernel_bit_is_unknown_not_a_state_that_asserts_one() {
        // What would have been `polling`.
        let polling_shaped = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: None,
            server_name: Some("0.pool.ntp.org".to_string()),
            server_address: Some("192.0.2.7".to_string()),
            sample: Some(sample(0, 2, 0.004)),
        };
        assert_eq!(classify(&polling_shaped), SyncStatus::Unknown);

        // What would have been `offline-degraded`.
        let degraded_shaped = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: None,
            ..TimesyncEvidence::default()
        };
        assert_eq!(classify(&degraded_shaped), SyncStatus::Unknown);

        // The source's own verdict still stands: it never rested on the bit.
        let unusable = TimesyncEvidence {
            sample: Some(sample(3, 2, 0.0)),
            ..polling_shaped.clone()
        };
        assert_eq!(classify(&unusable), SyncStatus::InvalidSource);

        // The controls: the same two shapes with the bit actually read.
        assert_eq!(
            classify(&TimesyncEvidence {
                ntp_synchronized: Some(false),
                ..polling_shaped.clone()
            }),
            SyncStatus::Polling
        );
        assert_eq!(
            classify(&TimesyncEvidence {
                ntp_synchronized: Some(false),
                ..degraded_shaped.clone()
            }),
            SyncStatus::OfflineDegraded
        );
    }

    /// The raw signal stays faithfully absent, and the `detail` says WHICH
    /// read did not answer.
    ///
    /// An absent `synchronized` member beside `unknown` is the pair that
    /// lets a reader tell "read and false" from "not read" — the whole point
    /// of not flattening the tri-state. Nothing is inferred to fill the gap:
    /// the sample and the server are still reported as observed, and neither
    /// is promoted into a claim about the clock.
    #[test]
    fn the_unread_signal_stays_absent_and_names_itself() {
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: None,
            server_name: Some("0.pool.ntp.org".to_string()),
            server_address: Some("192.0.2.7".to_string()),
            sample: Some(sample(0, 2, 0.004)),
        };
        let value = status_json(&evidence);
        assert_eq!(value["status"], "unknown");
        assert_eq!(value.get("synchronized"), None);
        // Observed evidence is still reported; it is simply not a state.
        assert_eq!(value["server"]["name"], "0.pool.ntp.org");
        assert_eq!(value["sample"]["stratum"], 2);

        // The two ways this state is reached name different services, so an
        // operator is not sent to look at a daemon that answered.
        let detail = value["detail"].as_str().expect("unknown carries a detail");
        assert!(detail.contains("timedated"), "{detail}");
        assert!(!detail.contains("timesyncd"), "{detail}");
        let off_bus = status_json(&TimesyncEvidence::default());
        assert!(
            off_bus["detail"]
                .as_str()
                .expect("unknown carries a detail")
                .contains("timesyncd")
        );
    }

    /// The step-versus-drift distinction, at timesyncd's own boundary: an
    /// offset past 0.4 s is a step, inside it a slew, and no sample means no
    /// `correction` member at all rather than a guessed one.
    #[test]
    fn the_correction_member_tells_a_step_from_a_slew() {
        let stepped = TimesyncEvidence {
            service_reachable: true,
            server_name: Some("s".to_string()),
            sample: Some(sample(0, 2, -3.2)),
            ..TimesyncEvidence::default()
        };
        assert_eq!(status_json(&stepped)["sample"]["correction"], "step");

        let slewed = TimesyncEvidence {
            sample: Some(sample(0, 2, 0.05)),
            ..stepped.clone()
        };
        assert_eq!(status_json(&slewed)["sample"]["correction"], "slew");

        let no_reply = TimesyncEvidence {
            service_reachable: true,
            ..TimesyncEvidence::default()
        };
        assert_eq!(status_json(&no_reply).get("sample"), None);
    }

    /// The served shape, key by key: the consumer is apid's time status route
    /// in another crate and reads these names off the bus string.
    #[test]
    fn the_status_json_carries_the_documented_members() {
        let evidence = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(true),
            server_name: Some("0.pool.ntp.org".to_string()),
            server_address: Some("192.0.2.7".to_string()),
            sample: Some(sample(0, 2, 0.012)),
        };
        let value = status_json(&evidence);
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["sample", "server", "status", "synchronized"]);
        assert_eq!(value["status"], "synchronized");
        assert_eq!(value["server"]["name"], "0.pool.ntp.org");
        assert_eq!(value["sample"]["stratum"], 2);

        // The unknown case names itself instead of dressing up as evidence.
        let unknown = status_json(&TimesyncEvidence::default());
        assert_eq!(unknown["status"], "unknown");
        assert!(unknown["detail"].as_str().unwrap().contains("timesyncd"));
    }

    /// The `NTPMessage` decode over a literal field slice: the offset is the
    /// standard four-timestamp calculation and the indexes are the ABI's.
    #[test]
    fn an_ntp_message_structure_decodes_into_a_sample() {
        // origin 1000, receive 1600, transmit 1700, destination 1100 (µs):
        // offset = ((1600-1000)+(1700-1100))/2 = 600 µs.
        let fields: Vec<Value<'_>> = vec![
            Value::U32(0),            // leap
            Value::U32(4),            // version
            Value::U32(4),            // mode
            Value::U32(2),            // stratum
            Value::I32(-24),          // precision
            Value::U64(0),            // root delay
            Value::U64(0),            // root dispersion
            Value::new(vec![0u8; 4]), // reference id
            Value::U64(1_000),        // origin
            Value::U64(1_600),        // receive
            Value::U64(1_700),        // transmit
            Value::U64(1_100),        // destination
            Value::Bool(false),       // spike
            Value::U64(7),            // packet count
            Value::U64(0),            // jitter
        ];
        let sample = parse_ntp_message(&fields).expect("the documented shape decodes");
        assert_eq!(sample.leap, 0);
        assert_eq!(sample.stratum, 2);
        assert!(!sample.spike);
        assert_eq!(sample.packet_count, 7);
        assert!((sample.offset_seconds - 0.0006).abs() < 1e-9);

        // A shape that is not the ABI's is absent evidence, not a panic.
        assert_eq!(parse_ntp_message(&[Value::U32(1)]), None);
        assert_eq!(parse_ntp_message(&[]), None);
    }

    #[test]
    fn a_server_address_formats_by_family() {
        assert_eq!(
            format_server_address(2, &[192, 0, 2, 7]).as_deref(),
            Some("192.0.2.7")
        );
        let mut v6 = vec![0u8; 16];
        v6[15] = 1;
        assert_eq!(format_server_address(10, &v6).as_deref(), Some("::1"));
        assert_eq!(format_server_address(2, &[1, 2]), None);
        assert_eq!(format_server_address(99, &[0; 4]), None);
    }

    /// PLAN-071 §7's second limb, and the case it exists for: a device with
    /// no STATE bind.
    ///
    /// The saved floor is the ONLY evidence an offline device has that its
    /// notion of now is being carried forward, so "no file at all" must read
    /// as no advance rather than as an advance nobody could disprove. The
    /// three answers are the three states an appliance is actually found in:
    /// STATE absent, STATE bound but timesyncd not yet writing, and the
    /// mechanism alive.
    #[test]
    fn the_saved_floor_advances_only_when_the_file_moved_after_this_boot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = dir.path().join("clock");
        // An hour into this boot, which is what /proc/uptime's first field
        // carries; the boot instant the comparison uses is derived from it,
        // so moving the uptime is how a test moves the boot rather than the
        // file.
        let an_hour_in = dir.path().join("uptime-3600");
        std::fs::write(&an_hour_in, "3600.00 7200.00\n").expect("seed uptime");
        // A machine that booted just now: a floor file already on the disk
        // was last written before this boot, not during it.
        let just_booted = dir.path().join("uptime-0");
        std::fs::write(&just_booted, "0.00 0.00\n").expect("seed uptime");

        assert!(
            !saved_floor_advanced(&clock, &an_hour_in),
            "a device with no STATE bind has no floor file, and an absent \
             signal is not an advance"
        );

        std::fs::write(&clock, b"").expect("seed the floor");
        assert!(
            !saved_floor_advanced(&clock, &just_booted),
            "STATE is bound but nothing has written the floor since boot"
        );
        assert!(
            saved_floor_advanced(&clock, &an_hour_in),
            "the floor moved during this boot: the mechanism is alive"
        );

        // An unreadable uptime is an unread signal, and the closed side here
        // is deferring the install.
        assert!(!saved_floor_advanced(&clock, &dir.path().join("absent")));
    }

    /// The predicate the automatic install is gated on, and the sentence an
    /// operator is given when it refuses.
    #[test]
    fn the_clock_is_believed_on_either_limb_and_the_refusal_names_both() {
        let degraded = ClockTrust {
            status: Some(SyncStatus::OfflineDegraded),
            floor_advanced: false,
        };
        assert!(!degraded.believed());
        let reason = degraded
            .untrusted_reason()
            .expect("an unbelieved clock says why");
        assert!(reason.contains("offline-degraded"), "{reason}");
        assert!(reason.contains(SAVED_CLOCK_PATH), "{reason}");
        assert!(
            reason.contains("checks and fetches are unaffected"),
            "the refusal must not read as a device that stopped discovering \
             updates: {reason}"
        );

        for believed in [
            ClockTrust {
                status: Some(SyncStatus::Synchronized),
                floor_advanced: false,
            },
            ClockTrust {
                status: Some(SyncStatus::OfflineDegraded),
                floor_advanced: true,
            },
        ] {
            assert!(believed.believed(), "{believed:?}");
            assert_eq!(believed.untrusted_reason(), None);
        }

        // Unread evidence is not a claim: a daemon with no observer at all
        // and no floor does not get to call its clock believable.
        let unread = ClockTrust {
            status: None,
            floor_advanced: false,
        };
        assert!(!unread.believed());
        assert!(
            unread
                .untrusted_reason()
                .expect("says why")
                .contains("`unknown`")
        );
    }
}
