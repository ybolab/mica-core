//! Power actions: reboot and power-off, executed through systemd.
//!
//! These are actions, not settings — nothing is persisted and nothing is
//! reconciled on boot, so they deliberately live outside `reconciler/`.
//!
//! The [`PowerControl`] trait exists so the bus layer can be unit-tested
//! against a mock, and so dry-run mode can substitute [`DryRunPower`], which
//! can never touch the host the daemon runs on.

/// Well-known bus name systemd owns.
pub const SYSTEMD_SERVICE: &str = "org.freedesktop.systemd1";
/// Object path of systemd's manager object.
pub const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
/// Interface carrying the manager-level power methods.
pub const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";
/// Manager member that reboots the machine.
pub const REBOOT_MEMBER: &str = "Reboot";
/// Manager member that powers the machine off.
pub const POWER_OFF_MEMBER: &str = "PowerOff";

/// Executes manager-level power actions on the host.
#[async_trait::async_trait]
pub trait PowerControl: Send + Sync {
    /// Reboot the machine.
    async fn reboot(&self) -> anyhow::Result<()>;
    /// Power the machine off.
    async fn power_off(&self) -> anyhow::Result<()>;
}

/// Production control calling [`SYSTEMD_MANAGER`] on the system bus.
///
/// The bus connection is created lazily inside the call, so constructing this
/// never touches the host.
#[derive(Default)]
pub struct Systemd {
    /// Explicit bus address; `None` selects the host system bus.
    address: Option<String>,
}

impl Systemd {
    /// Control talking to systemd on the host system bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Control talking to systemd on an explicit bus address.
    ///
    /// Test-only: it exists so the production call path can be driven against
    /// a fake systemd on a private bus, never against the host.
    #[cfg(test)]
    pub fn at_address(address: &str) -> Self {
        Self {
            address: Some(address.to_string()),
        }
    }

    /// Invoke `member` on systemd's manager object.
    async fn call(&self, member: &str) -> anyhow::Result<()> {
        let connection = match &self.address {
            Some(address) => {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await?
            }
            None => zbus::Connection::system().await?,
        };
        connection
            .call_method(
                Some(SYSTEMD_SERVICE),
                SYSTEMD_PATH,
                Some(SYSTEMD_MANAGER),
                member,
                &(),
            )
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PowerControl for Systemd {
    async fn reboot(&self) -> anyhow::Result<()> {
        self.call(REBOOT_MEMBER).await
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        self.call(POWER_OFF_MEMBER).await
    }
}

/// No-op [`PowerControl`] installed in dry-run mode.
///
/// Dry-run is what the integration tests run the daemon under, so this is the
/// reason no test can power off the build host: the production [`Systemd`]
/// control is never constructed there.
pub struct DryRunPower;

#[async_trait::async_trait]
impl PowerControl for DryRunPower {
    async fn reboot(&self) -> anyhow::Result<()> {
        tracing::info!("dry run: reboot request not forwarded to systemd");
        Ok(())
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        tracing::info!("dry run: power-off request not forwarded to systemd");
        Ok(())
    }
}

/// Recording [`PowerControl`] for the bus-layer unit tests.
#[cfg(test)]
pub struct MockPower {
    /// Shared with the test so the log stays readable after the mock is moved
    /// into the service.
    pub calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl PowerControl for MockPower {
    async fn reboot(&self) -> anyhow::Result<()> {
        self.calls.lock().expect("mock lock").push("reboot".into());
        Ok(())
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        self.calls
            .lock()
            .expect("mock lock")
            .push("power_off".into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Kills the wrapped child on drop, including on panic.
    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Locate `dbus-daemon`: `/usr/bin/dbus-daemon` first, then `$PATH`.
    fn find_dbus_daemon() -> Option<PathBuf> {
        let fixed = PathBuf::from("/usr/bin/dbus-daemon");
        if fixed.exists() {
            return Some(fixed);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("dbus-daemon"))
            .find(|candidate| candidate.exists())
    }

    /// Stand-in for systemd on a private bus, recording the member invoked.
    ///
    /// The member names are spelled out rather than derived, so this fake
    /// cannot silently agree with a typo in the production constants.
    struct FakeSystemd {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeSystemd {
        #[zbus(name = "Reboot")]
        async fn reboot(&self) {
            self.calls.lock().expect("fake lock").push("Reboot".into());
        }

        #[zbus(name = "PowerOff")]
        async fn power_off(&self) {
            self.calls
                .lock()
                .expect("fake lock")
                .push("PowerOff".into());
        }
    }

    /// Drives the PRODUCTION [`Systemd`] call path against a fake systemd on a
    /// private session bus. A wrong service name, object path, interface or
    /// member fails to dispatch and this test goes red — which a mock handed
    /// the same wrong name would not.
    #[tokio::test(flavor = "multi_thread")]
    async fn production_path_dispatches_to_the_real_systemd_names() -> anyhow::Result<()> {
        let Some(dbus_daemon) = find_dbus_daemon() else {
            eprintln!(
                "skipping production_path_dispatches_to_the_real_systemd_names: dbus-daemon not found"
            );
            return Ok(());
        };

        // Private session bus; never the host system bus.
        let mut bus_child = Command::new(dbus_daemon)
            .args(["--session", "--print-address=1", "--nofork"])
            .stdout(Stdio::piped())
            .spawn()?;
        let bus_stdout = bus_child.stdout.take().expect("piped stdout");
        let _bus_guard = ChildGuard(bus_child);
        let mut address = String::new();
        BufReader::new(bus_stdout).read_line(&mut address)?;
        let address = address.trim().to_string();
        anyhow::ensure!(!address.is_empty(), "dbus-daemon printed no address");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let _server = zbus::connection::Builder::address(address.as_str())?
            .name("org.freedesktop.systemd1")?
            .serve_at(
                "/org/freedesktop/systemd1",
                FakeSystemd {
                    calls: Arc::clone(&calls),
                },
            )?
            .build()
            .await?;

        let systemd = Systemd::at_address(&address);

        systemd.reboot().await?;
        assert_eq!(*calls.lock().expect("lock"), vec!["Reboot".to_string()]);

        systemd.power_off().await?;
        assert_eq!(
            *calls.lock().expect("lock"),
            vec!["Reboot".to_string(), "PowerOff".to_string()]
        );

        // Dispatch really is name-sensitive: a member the fake does not serve
        // errors instead of quietly succeeding.
        assert!(systemd.call("Halt").await.is_err());
        assert_eq!(
            *calls.lock().expect("lock"),
            vec!["Reboot".to_string(), "PowerOff".to_string()]
        );

        Ok(())
    }

    #[tokio::test]
    async fn dry_run_control_never_calls_out() -> anyhow::Result<()> {
        // No bus exists in this test, so a control that tried to reach systemd
        // would fail; both calls succeeding proves nothing left the process.
        DryRunPower.reboot().await?;
        DryRunPower.power_off().await?;
        Ok(())
    }
}
