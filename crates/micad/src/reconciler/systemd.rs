//! Shared systemd unit control: the operations a reconciler needs to converge a
//! named unit's runtime state. Deliberately unit-name-generic — the sshd
//! reconciler drives `dropbear.service` through it and the WiFi client and AP
//! reconcilers drive `wpa_supplicant@…` and `hostapd` through the same trait.
//!
//! Enablement is runtime-scoped. `EnableUnitFiles` with `runtime = false`
//! writes symlinks under `/etc/systemd/system`, which the mos read-only root
//! does not offer: `/etc` lives on the dm-verity squashfs. Runtime scope
//! writes to `/run/systemd/system`, which always works, and micad reconciles the
//! whole settings tree on every start, so the unit returns to its configured
//! state each boot without a persisted symlink. `stop` is the authoritative
//! disablement at runtime; `disable` keeps `systemctl is-enabled` honest for
//! the current boot.

use anyhow::Result;
use std::time::Duration;
use tokio::sync::OnceCell;

/// systemd's well-known bus name.
const MANAGER_DESTINATION: &str = "org.freedesktop.systemd1";
/// Object path of systemd's manager object.
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
/// Manager interface carrying the unit lifecycle methods.
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
/// Unit interface carrying the `ActiveState` property.
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
/// Standard D-Bus property interface.
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
/// Job mode for start/stop/restart: queue the job, displacing
/// conflicting ones.
const JOB_MODE: &str = "replace";
/// Upper bound for connecting to systemd or waiting for one D-Bus reply.
const SYSTEMD_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Controls the runtime state of a systemd unit by name.
///
/// The state readers exist so a reconciler can converge rather than command:
/// it reads first and only issues the calls that change something, which is
/// what makes a repeated `apply` a genuine no-op.
#[async_trait::async_trait]
pub trait UnitControl: Send + Sync {
    /// `ActiveState` of `unit` — one of systemd's `active`, `activating`,
    /// `reloading`, `deactivating`, `inactive`, `failed`.
    ///
    /// # Errors
    ///
    /// Returns an error when the unit cannot be loaded or the bus call fails.
    async fn active_state(&self, unit: &str) -> Result<String>;

    /// Unit file enablement state of `unit` — one of systemd's `enabled`,
    /// `enabled-runtime`, `disabled`, `static`, `masked`, `linked`, ….
    ///
    /// # Errors
    ///
    /// Returns an error when the unit file cannot be found or the bus call
    /// fails.
    async fn unit_file_state(&self, unit: &str) -> Result<String>;

    /// Start `unit`.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails or systemd refuses the job.
    async fn start(&self, unit: &str) -> Result<()>;

    /// Stop `unit`.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails or systemd refuses the job.
    async fn stop(&self, unit: &str) -> Result<()>;

    /// Restart `unit`, so a rewritten configuration file takes effect.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails or systemd refuses the job.
    async fn restart(&self, unit: &str) -> Result<()>;

    /// Clear `unit`'s failed state, and with it the start rate limit a
    /// repeatedly-failing unit accumulates.
    ///
    /// The equivalent of `systemctl reset-failed <unit>`, and it exists for the
    /// start limit rather than for cosmetics. Once a unit exceeds its
    /// `StartLimitBurst` within `StartLimitIntervalSec`, systemd refuses every
    /// further start job, from any caller, until the window elapses or the
    /// failure is reset. A reconciler that converges by reading
    /// [`UnitControl::active_state`] and starting whatever is not active sees
    /// `failed`, issues the start and has it refused, so the operator's fix
    /// would take effect only once the window expired and something triggered
    /// another apply. A no-op on a unit that is not failed, as `systemctl
    /// reset-failed` is; callers still read the state first, so the call log
    /// says which unit was in trouble.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails.
    async fn reset_failed(&self, unit: &str) -> Result<()>;

    /// Enable `unit` for this boot.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails or the unit file carries no
    /// install information.
    async fn enable(&self, unit: &str) -> Result<()>;

    /// Disable `unit` for this boot.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails.
    async fn disable(&self, unit: &str) -> Result<()>;

    /// Re-run every systemd generator and reload the unit tree.
    ///
    /// The equivalent of `systemctl daemon-reload`, and unlike the methods
    /// above it names no unit because it acts on all of them. It exists for
    /// GENERATORS: Quadlet is one, so a `.container` file only becomes a
    /// service when systemd re-runs it, and a reconciler that mounted the
    /// directory holding those files without reloading would leave the mount
    /// correct and the units nonexistent -- with nothing reporting a problem.
    ///
    /// # Errors
    ///
    /// Returns an error when the bus call fails.
    async fn daemon_reload(&self) -> Result<()>;
}

/// True when `state` is an [`UnitControl::active_state`] value that means the
/// unit is running or on its way up, i.e. starting it again would be
/// redundant.
#[must_use]
pub fn is_active(state: &str) -> bool {
    matches!(state, "active" | "activating" | "reloading")
}

/// True when `state` is an [`UnitControl::unit_file_state`] value that means
/// the unit is already enabled, at either scope.
///
/// `static` counts as enabled: such a unit has no `[Install]` section, so
/// enabling it is both impossible and unnecessary.
#[must_use]
pub fn is_enabled(state: &str) -> bool {
    matches!(state, "enabled" | "enabled-runtime" | "static")
}

/// Production [`UnitControl`] calling `org.freedesktop.systemd1` on the system
/// bus.
///
/// The bus connection is created lazily on first use and then retained for the
/// executor's lifetime, so constructing this executor never touches the host
/// and one reconcile does not repeat the full bus authentication handshake for
/// every unit operation.
pub struct Systemd {
    connection: OnceCell<zbus::Connection>,
}

impl Systemd {
    /// A disconnected executor. The first call establishes its one connection.
    pub fn new() -> Self {
        Self {
            connection: OnceCell::new(),
        }
    }

    async fn connection(&self) -> Result<&zbus::Connection> {
        Ok(self
            .connection
            .get_or_try_init(zbus::Connection::system)
            .await?)
    }

    /// Call `method` on systemd's manager object with `body`.
    async fn manager_call<B>(&self, method: &str, body: &B) -> Result<zbus::Message>
    where
        B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        let call = async {
            let connection = self.connection().await?;
            let reply = connection
                .call_method(
                    Some(MANAGER_DESTINATION),
                    MANAGER_PATH,
                    Some(MANAGER_INTERFACE),
                    method,
                    body,
                )
                .await?;
            anyhow::Ok(reply)
        };
        tokio::time::timeout(SYSTEMD_CALL_TIMEOUT, call)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "systemd manager method {method} did not answer within {SYSTEMD_CALL_TIMEOUT:?}"
                )
            })?
    }
}

impl Default for Systemd {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl UnitControl for Systemd {
    async fn active_state(&self, unit: &str) -> Result<String> {
        // LoadUnit rather than GetUnit: GetUnit fails outright on a unit
        // systemd has not loaded yet, which is the normal state of a service
        // that has never been started this boot.
        let reply = self.manager_call("LoadUnit", &(unit,)).await?;
        let unit_path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;

        let call = async {
            self.connection()
                .await?
                .call_method(
                    Some(MANAGER_DESTINATION),
                    &unit_path,
                    Some(PROPERTIES_INTERFACE),
                    "Get",
                    &(UNIT_INTERFACE, "ActiveState"),
                )
                .await
                .map_err(anyhow::Error::from)
        };
        let reply = tokio::time::timeout(SYSTEMD_CALL_TIMEOUT, call)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "systemd ActiveState for {unit} did not answer within {SYSTEMD_CALL_TIMEOUT:?}"
                )
            })??;
        let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
        Ok(String::try_from(value)?)
    }

    async fn unit_file_state(&self, unit: &str) -> Result<String> {
        let reply = self.manager_call("GetUnitFileState", &(unit,)).await?;
        Ok(reply.body().deserialize()?)
    }

    async fn start(&self, unit: &str) -> Result<()> {
        self.manager_call("StartUnit", &(unit, JOB_MODE)).await?;
        Ok(())
    }

    async fn stop(&self, unit: &str) -> Result<()> {
        self.manager_call("StopUnit", &(unit, JOB_MODE)).await?;
        Ok(())
    }

    async fn restart(&self, unit: &str) -> Result<()> {
        self.manager_call("RestartUnit", &(unit, JOB_MODE)).await?;
        Ok(())
    }

    async fn reset_failed(&self, unit: &str) -> Result<()> {
        // ResetFailedUnit and NOT ResetFailed: the latter takes no argument
        // and clears the failed state of EVERY unit on the system, which is
        // not a caller reconciling one service's to erase. It also takes no
        // job mode -- it queues no job, it edits the unit's bookkeeping.
        self.manager_call("ResetFailedUnit", &(unit,)).await?;
        Ok(())
    }

    async fn enable(&self, unit: &str) -> Result<()> {
        // (files, runtime, force): runtime = true keeps the symlinks in /run,
        // see the module docs. force = true replaces a stale symlink rather
        // than failing on it.
        self.manager_call("EnableUnitFiles", &(&[unit][..], true, true))
            .await?;
        Ok(())
    }

    async fn disable(&self, unit: &str) -> Result<()> {
        self.manager_call("DisableUnitFiles", &(&[unit][..], true))
            .await?;
        Ok(())
    }

    async fn daemon_reload(&self) -> Result<()> {
        self.manager_call("Reload", &()).await?;
        Ok(())
    }
}

/// Recording [`UnitControl`] mock for reconciler tests.
///
/// Lives here rather than in each reconciler's test module because every
/// reconciler that drives a unit needs the same one.
#[cfg(test)]
pub mod mock {
    use std::sync::Mutex;

    use anyhow::Result;

    /// Mutable half of [`MockUnitControl`].
    struct State {
        active: String,
        file: String,
        calls: Vec<String>,
        /// Per-unit overrides of `active`/`file`.
        ///
        /// A single pair was enough while every reconciler drove exactly one
        /// unit. `ContainerReconciler` drives a mount AND the units Quadlet
        /// generated behind it, and the ORDER it touches them in is the thing
        /// worth asserting -- which a mock that answers identically for every
        /// unit cannot express. `new` keeps its meaning: the pair it takes is
        /// the answer for any unit not named here.
        active_by_unit: std::collections::BTreeMap<String, String>,
        file_by_unit: std::collections::BTreeMap<String, String>,
        /// Units whose [`super::UnitControl::start`] is refused.
        ///
        /// Per-unit for the same reason the two maps above are: the callers
        /// that meet a refused start drive more than one unit, and what is
        /// worth asserting is that the OTHERS are still driven afterwards. A
        /// flag on the mock as a whole could not express that.
        start_refused: std::collections::BTreeSet<String>,
    }

    /// [`super::UnitControl`] that records mutating calls and models the state
    /// transitions they would cause, so a second `apply` sees the world the
    /// first one left behind.
    pub struct MockUnitControl {
        state: Mutex<State>,
    }

    impl MockUnitControl {
        /// Mock starting from `active` ([`super::UnitControl::active_state`])
        /// and `file` ([`super::UnitControl::unit_file_state`]).
        pub fn new(active: &str, file: &str) -> Self {
            Self {
                state: Mutex::new(State {
                    active: active.to_string(),
                    file: file.to_string(),
                    calls: Vec::new(),
                    active_by_unit: std::collections::BTreeMap::new(),
                    file_by_unit: std::collections::BTreeMap::new(),
                    start_refused: std::collections::BTreeSet::new(),
                }),
            }
        }

        /// Answer `state` for `unit`'s [`super::UnitControl::active_state`],
        /// overriding the constructor's default for that unit only.
        pub fn set_active_state(&self, unit: &str, state: &str) {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .active_by_unit
                .insert(unit.to_string(), state.to_string());
        }

        /// Answer `state` for `unit`'s [`super::UnitControl::unit_file_state`].
        pub fn set_unit_file_state(&self, unit: &str, state: &str) {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .file_by_unit
                .insert(unit.to_string(), state.to_string());
        }

        /// Refuse `unit`'s [`super::UnitControl::start`], the shape of a unit
        /// systemd will not start -- in practice one that has exhausted its
        /// `StartLimitBurst` and is in cool-off for the rest of its
        /// `StartLimitIntervalSec`.
        ///
        /// The attempt is still recorded, so a test can assert both that the
        /// start was
        /// tried and what the caller did after it was refused. Per-unit rather
        /// than a constructor, because the assertion worth making is about the
        /// units that were NOT refused.
        pub fn refuse_start(&self, unit: &str) {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.start_refused.insert(unit.to_string());
        }

        /// Mutating calls seen so far, in order, as `"<verb> <unit>"`.
        ///
        /// State reads are deliberately not recorded: a reconciler is expected
        /// to read freely and only the writes are what "no redundant state
        /// change" is about.
        pub fn calls(&self) -> Vec<String> {
            match self.state.lock() {
                Ok(state) => state.calls.clone(),
                Err(poisoned) => poisoned.into_inner().calls.clone(),
            }
        }

        /// Record `verb` against `unit` WITHOUT applying its transition.
        ///
        /// For a call that was made and then refused: systemd rejecting a
        /// start job leaves the unit exactly where it was, and a mock that
        /// moved it to `active` anyway would make the caller's next read lie
        /// about a unit that never ran.
        fn record_refused(&self, verb: &str, unit: &str) {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.calls.push(format!("{verb} {unit}"));
        }

        /// Record `verb` against `unit` and apply its state transition.
        fn record(&self, verb: &str, unit: &str) {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.calls.push(format!("{verb} {unit}"));
            // The transition lands on the per-unit entry as well, so a second
            // apply sees what the first left behind for THAT unit rather than
            // for whichever unit was touched last.
            // PER-UNIT ONLY: the shared `active`/`file` defaults are NOT
            // moved. Moving them would make starting ONE unit read back as
            // active for every unit the test never named, and a reconciler
            // that then skipped starting a container would look correct. The
            // constructor's pair stays what it is: the answer for units
            // nothing has touched.
            match verb {
                "start" | "restart" => {
                    state
                        .active_by_unit
                        .insert(unit.to_string(), "active".to_string());
                }
                "stop" => {
                    state
                        .active_by_unit
                        .insert(unit.to_string(), "inactive".to_string());
                }
                "enable" => {
                    state
                        .file_by_unit
                        .insert(unit.to_string(), "enabled-runtime".to_string());
                }
                "disable" => {
                    state
                        .file_by_unit
                        .insert(unit.to_string(), "disabled".to_string());
                }
                // What `systemctl reset-failed` does: a FAILED unit becomes
                // inactive, and a unit in any other state is untouched. The
                // effective state is what decides, not just the per-unit
                // override, so a mock constructed with a shared default of
                // "failed" models it too.
                "reset-failed" => {
                    let effective = state
                        .active_by_unit
                        .get(unit)
                        .unwrap_or(&state.active)
                        .clone();
                    if effective == "failed" {
                        state
                            .active_by_unit
                            .insert(unit.to_string(), "inactive".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    #[async_trait::async_trait]
    impl super::UnitControl for MockUnitControl {
        async fn active_state(&self, unit: &str) -> Result<String> {
            let state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            Ok(state
                .active_by_unit
                .get(unit)
                .cloned()
                .unwrap_or_else(|| state.active.clone()))
        }

        async fn unit_file_state(&self, unit: &str) -> Result<String> {
            let state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            Ok(state
                .file_by_unit
                .get(unit)
                .cloned()
                .unwrap_or_else(|| state.file.clone()))
        }

        async fn start(&self, unit: &str) -> Result<()> {
            let refused = {
                let state = match self.state.lock() {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.start_refused.contains(unit)
            };
            if refused {
                self.record_refused("start", unit);
                return Err(anyhow::anyhow!(
                    "Job for {unit} failed: start request repeated too quickly"
                ));
            }
            self.record("start", unit);
            Ok(())
        }

        async fn stop(&self, unit: &str) -> Result<()> {
            self.record("stop", unit);
            Ok(())
        }

        async fn restart(&self, unit: &str) -> Result<()> {
            self.record("restart", unit);
            Ok(())
        }

        async fn reset_failed(&self, unit: &str) -> Result<()> {
            self.record("reset-failed", unit);
            Ok(())
        }

        async fn enable(&self, unit: &str) -> Result<()> {
            self.record("enable", unit);
            Ok(())
        }

        async fn disable(&self, unit: &str) -> Result<()> {
            self.record("disable", unit);
            Ok(())
        }

        async fn daemon_reload(&self) -> Result<()> {
            // Recorded with a unit name of "-" rather than "": a test asserting
            // on the call log reads `daemon-reload -`, and an empty second
            // field would render as a trailing space that is easy to miss in a
            // diff of expected calls.
            self.record("daemon-reload", "-");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_states_that_mean_running_or_coming_up() {
        assert!(is_active("active"));
        assert!(is_active("activating"));
        assert!(is_active("reloading"));
        assert!(!is_active("inactive"));
        assert!(!is_active("deactivating"));
        assert!(!is_active("failed"));
    }

    #[test]
    fn enablement_states_that_mean_already_enabled() {
        assert!(is_enabled("enabled"));
        assert!(is_enabled("enabled-runtime"));
        assert!(is_enabled("static"));
        assert!(!is_enabled("disabled"));
        assert!(!is_enabled("masked"));
        assert!(!is_enabled("linked"));
    }

    #[tokio::test]
    async fn mock_records_mutations_and_models_their_transitions() {
        use mock::MockUnitControl;

        let control = MockUnitControl::new("inactive", "disabled");
        control.enable("u.service").await.unwrap();
        control.start("u.service").await.unwrap();

        assert_eq!(control.active_state("u.service").await.unwrap(), "active");
        assert_eq!(
            control.unit_file_state("u.service").await.unwrap(),
            "enabled-runtime"
        );

        control.stop("u.service").await.unwrap();
        control.disable("u.service").await.unwrap();

        assert_eq!(control.active_state("u.service").await.unwrap(), "inactive");
        assert_eq!(
            control.unit_file_state("u.service").await.unwrap(),
            "disabled"
        );
        assert_eq!(
            control.calls(),
            vec![
                "enable u.service".to_string(),
                "start u.service".to_string(),
                "stop u.service".to_string(),
                "disable u.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn mock_reset_failed_clears_a_failed_unit_and_leaves_others_alone() {
        use mock::MockUnitControl;

        let control = MockUnitControl::new("active", "enabled");
        control.set_active_state("broken.service", "failed");

        control.reset_failed("broken.service").await.unwrap();
        control.reset_failed("healthy.service").await.unwrap();

        assert_eq!(
            control.active_state("broken.service").await.unwrap(),
            "inactive",
            "reset-failed clears the failure, which is what unblocks the start after it"
        );
        assert_eq!(
            control.active_state("healthy.service").await.unwrap(),
            "active",
            "reset-failed on a unit that is not failed does nothing to it"
        );
        assert_eq!(
            control.calls(),
            vec![
                "reset-failed broken.service".to_string(),
                "reset-failed healthy.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn mock_can_refuse_one_units_start_without_touching_another() {
        use mock::MockUnitControl;

        let control = MockUnitControl::new("inactive", "enabled");
        control.refuse_start("limited.service");

        let error = control.start("limited.service").await.unwrap_err();
        control.start("other.service").await.unwrap();

        assert!(
            error.to_string().contains("repeated too quickly"),
            "unexpected error: {error}"
        );
        // Refused, so the unit did NOT run: a mock that marked it active here
        // would let a caller's next read claim a broker that never started.
        assert_eq!(
            control.active_state("limited.service").await.unwrap(),
            "inactive"
        );
        assert_eq!(
            control.active_state("other.service").await.unwrap(),
            "active"
        );
        // The attempt is recorded either way, so a test can assert both that
        // the start was tried and what happened after it was refused.
        assert_eq!(
            control.calls(),
            vec![
                "start limited.service".to_string(),
                "start other.service".to_string(),
            ]
        );
    }
}
