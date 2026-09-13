//! Hostname reconciler: writes `/etc/hostname` and sets the running hostname.
//!
//! NOT SetStaticHostname. hostnamed cannot write /etc/hostname on this
//! appliance -- its ProtectSystem=strict namespace has no writable /etc,
//! because /etc is a dm-verity squashfs and the one writable path in it is a
//! bind mount that does not survive into that namespace. The file is micad's to
//! write, the same way the sshd reconciler owns its drop-in.

use anyhow::Context;
use micad_settings::Settings;
use serde_json::json;

use super::Reconciler;

/// Executes the hostname change on the host.
#[async_trait::async_trait]
pub trait HostnameExecutor: Send + Sync {
    /// Set the system's static hostname to `name`.
    async fn set_static_hostname(&self, name: &str) -> anyhow::Result<()>;
}

/// Production executor calling `org.freedesktop.hostname1` on the system bus.
///
/// The bus connection is created lazily inside the call, so constructing this
/// executor never touches the host.
/// Holds the path it writes rather than reading an env var: the crate denies
/// `unsafe`, so a test cannot set one, and a path only production knows is a
/// path no test exercises.
pub struct Hostnamed {
    path: std::path::PathBuf,
}

impl Hostnamed {
    /// Executor writing the system's `/etc/hostname`.
    #[must_use]
    pub fn production() -> Self {
        Self {
            path: std::path::PathBuf::from(DEFAULT_HOSTNAME_PATH),
        }
    }

    /// Executor writing `path`, for tests.
    ///
    /// `cfg(test)` rather than `allow(dead_code)`: it exists only for tests,
    /// and an allow would also silence the day it stops being used at all.
    #[cfg(test)]
    #[must_use]
    pub fn with_path(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

/// Where the static hostname lives. A STATE-backed bind (etc-hostname.mount),
/// so micad can write it; the read-only root underneath cannot be written by
/// anyone.
const DEFAULT_HOSTNAME_PATH: &str = "/etc/hostname";
/// Mode of the hostname file: world-readable, owner-writable, as Debian ships
/// it. Anything that resolves the machine's own name reads it.
const HOSTNAME_MODE: u32 = 0o644;

#[async_trait::async_trait]
impl HostnameExecutor for Hostnamed {
    /// Write the file, then set the RUNNING hostname; do not ask hostnamed to
    /// write anything.
    ///
    /// SetStaticHostname was what this called, and on this appliance it can
    /// never succeed. systemd-hostnamed runs with ProtectSystem=strict, which
    /// remounts the whole hierarchy read-only inside its namespace and then
    /// re-mounts /etc read-write -- and /etc here is a dm-verity squashfs, so
    /// there is nothing to re-mount read-write. The one writable thing is the
    /// /etc/hostname bind, and it does not survive into that namespace.
    ///
    /// Measured on a QEMU boot: every apply logged
    ///   org.freedesktop.hostname1.ReadOnlyFilesystem: /etc/hostname is in a
    ///   read-only filesystem
    /// and the device's hostname was right anyway, because
    /// mica-apply-hostname.service sets it from the file at boot. So the
    /// failure was invisible in every way except the log: what did not work
    /// was CHANGING the hostname at runtime through the management API or bus.
    ///
    /// micad writes the file itself -- the same thing the sshd reconciler does
    /// with its drop-in -- and asks hostnamed only for the transient hostname,
    /// which is a kernel setting and touches no file.
    async fn set_static_hostname(&self, name: &str) -> anyhow::Result<()> {
        let path = self.path.display().to_string();
        let mut body = String::with_capacity(name.len() + 1);
        body.push_str(name);
        body.push('\n');
        // crate::fswrite decides between rename and in-place by asking the
        // filesystem whether the target is a mount point. /etc/hostname is the
        // one path in micad that is, and every other renderer needs the rename;
        // choosing per caller is how the first version of this reconciler came
        // to use a temp file that cannot be created and a rename that cannot
        // succeed.
        crate::fswrite::write_config(self.path.as_path(), &body, HOSTNAME_MODE)
            .with_context(|| format!("write {path}"))?;

        let connection = zbus::Connection::system().await?;
        connection
            .call_method(
                Some("org.freedesktop.hostname1"),
                "/org/freedesktop/hostname1",
                Some("org.freedesktop.hostname1"),
                "SetHostname",
                &(name, false),
            )
            .await?;
        Ok(())
    }
}

/// Reconciler for the `hostname` settings subtree.
pub struct HostnameReconciler<E: HostnameExecutor> {
    executor: E,
}

impl<E: HostnameExecutor> HostnameReconciler<E> {
    /// Create a hostname reconciler applying changes through `executor`.
    pub fn new(executor: E) -> Self {
        Self { executor }
    }
}

#[async_trait::async_trait]
impl<E: HostnameExecutor> Reconciler for HostnameReconciler<E> {
    fn name(&self) -> &'static str {
        "hostname"
    }

    fn subtree(&self) -> &'static str {
        "hostname"
    }

    async fn apply(&self, settings: &Settings) -> anyhow::Result<serde_json::Value> {
        self.executor
            .set_static_hostname(&settings.hostname)
            .await?;
        Ok(json!({ "hostname": settings.hostname }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct MockExecutor {
        calls: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl HostnameExecutor for MockExecutor {
        async fn set_static_hostname(&self, name: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(name.to_string());
            if self.fail {
                anyhow::bail!("hostnamed unavailable");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn apply_sets_hostname_once_and_returns_live_state() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reconciler = HostnameReconciler::new(MockExecutor {
            calls: Arc::clone(&calls),
            fail: false,
        });
        let settings = Settings {
            hostname: "mos-test".to_string(),
            ..Settings::default()
        };

        let state = reconciler.apply(&settings).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), vec!["mos-test".to_string()]);
        assert_eq!(state, json!({ "hostname": "mos-test" }));
        assert_eq!(reconciler.name(), "hostname");
        assert_eq!(reconciler.subtree(), "hostname");
    }

    #[tokio::test]
    async fn apply_propagates_executor_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reconciler = HostnameReconciler::new(MockExecutor {
            calls: Arc::clone(&calls),
            fail: true,
        });
        let settings = Settings::default();

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(err.to_string().contains("hostnamed unavailable"));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    /// The production executor writes the file rather than asking hostnamed to.
    ///
    /// It cannot reach a bus in a unit test, so the D-Bus half fails and the
    /// call returns Err -- the assertion is about what is ON DISK by then. A
    /// test that only checked the Result would pass for an implementation that
    /// wrote nothing.
    #[tokio::test]
    async fn the_production_executor_writes_the_hostname_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hostname");
        let _ = Hostnamed::with_path(path.clone())
            .set_static_hostname("edge-42")
            .await;

        let written = std::fs::read_to_string(&path).expect("the file must exist");
        assert_eq!(
            written, "edge-42\n",
            "the hostname file must hold exactly the name and a newline: mica-apply-hostname.service runs `hostname -F` on it at every boot"
        );
    }

    /// Nothing is written beside the file, because nothing can be: the
    /// directory is read-only on a device.
    #[tokio::test]
    async fn nothing_is_written_beside_the_hostname_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hostname");
        let _ = Hostnamed::with_path(path.clone())
            .set_static_hostname("edge-42")
            .await;

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "hostname")
            .collect();
        assert!(
            leftovers.is_empty(),
            "the atomic write left files behind: {leftovers:?}"
        );
    }

    /// The reconciler renders the file; WHICH write strategy is used is
    /// crate::fswrite's decision and is tested there.
    ///
    /// The assertion is on the CONTENT, not on the inode. Inode survival is
    /// true on a device -- /etc/hostname is a mount point, so fswrite writes
    /// in place -- and false in a temp directory, where fswrite correctly
    /// renames. A test that encodes the strategy rather than the outcome fails
    /// when the strategy is chosen correctly for the test's own environment.
    #[tokio::test]
    async fn the_hostname_file_holds_exactly_the_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hostname");
        std::fs::write(&path, "old\n").expect("seed the file");

        let _ = Hostnamed::with_path(path.clone())
            .set_static_hostname("edge-42")
            .await;

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "edge-42\n",
            "mica-apply-hostname.service runs `hostname -F` on this file at every boot"
        );
    }
}
