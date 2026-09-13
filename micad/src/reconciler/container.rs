//! Reconciler for the `container` settings subtree.
//!
//! What this switch operates is not a service. The engine is daemonless:
//! `podman run` forks `conmon`, which execs `crun`, and nothing stays resident;
//! mica-podman does not run upstream's `make install.systemd`, so the image
//! contains no podman unit at all -- no `podman.socket` to leave masked and no
//! service to leave stopped.
//!
//! The real gate is the Quadlet directory. Quadlet is a systemd generator: at
//! every daemon-reload it reads `/etc/containers/systemd` and turns each
//! `.container` file into a service, creating that service's `.wants` symlink
//! itself when the file carries an `[Install]` section. With `enabled = false`
//! the STATE bind is not mounted, `/etc/containers/systemd` is the empty
//! directory inside the read-only verity root, Quadlet parses nothing and no
//! container unit exists; with `enabled = true` the bind is mounted, Quadlet
//! parses what the integrator put on STATE, and the units it generates start.
//! The daemon-reload is not optional: mounting the directory changes nothing by
//! itself, because generators run at boot and on reload, so without one the
//! mount is correct, the files are visible, and no unit exists -- a state in
//! which every individual step succeeded.

use std::path::PathBuf;

use anyhow::Result;
use micad_settings::Settings;

use super::Reconciler;
use super::systemd::{Systemd, UnitControl, is_active, is_enabled};

/// Mount unit binding `/etc/containers/systemd` from STATE.
///
/// This is the switch. It ships installed and not enabled: the image must not
/// carry its `local-fs.target.wants` symlink, or the Quadlet directory is
/// bound at every boot regardless of the setting, and anything able to write
/// STATE has a root-capable container at the next reboot with no operator
/// decision anywhere in the path.
pub const QUADLET_MOUNT_UNIT: &str = "etc-containers-systemd.mount";

/// Directory Quadlet reads. Measured from `quadlet --dryrun`, which prints its
/// own search path as `[/run/containers/systemd /etc/containers/systemd
/// /usr/share/containers/systemd]`. `/usr/local/lib/systemd/system` is not on
/// that path and Quadlet never looks at it.
const DEFAULT_QUADLET_DIR: &str = "/etc/containers/systemd";
/// Override for tests.
pub const QUADLET_DIR_ENV: &str = "MICA_QUADLET_DIR";

/// Where systemd leaves what its generators produced.
const DEFAULT_GENERATOR_DIR: &str = "/run/systemd/generator";
/// Override for tests.
pub const GENERATOR_DIR_ENV: &str = "MICA_SYSTEMD_GENERATOR_DIR";

/// Marker identifying a unit as one Quadlet generated for this image.
///
/// Content, not filename. Quadlet's file-name mapping is its own and has
/// grown cases (`.pod` becomes `<name>-pod.service`, `.volume` becomes
/// `<name>-volume.service`); a reconciler that reimplemented that table would
/// fail to stop exactly the unit types it had not heard of, and would fail
/// SILENTLY, leaving a root-capable container running while the settings tree
/// says containers are off. Matching on the podman path this image installs
/// asks the generated file what it does instead of predicting its name.
const GENERATED_UNIT_MARKER: &str = "/usr/bin/podman";

/// Reconciler for the `container` subtree.
pub struct ContainerReconciler<C: UnitControl> {
    quadlet_dir: PathBuf,
    generator_dir: PathBuf,
    control: C,
}

impl<C: UnitControl> ContainerReconciler<C> {
    /// Reconciler reading `quadlet_dir`, scanning `generator_dir` for units
    /// Quadlet produced, and driving the mount through `control`.
    ///
    /// Both paths are parameters so tests run inside a temporary directory and
    /// never read the host's real generator output.
    pub fn new(quadlet_dir: PathBuf, generator_dir: PathBuf, control: C) -> Self {
        Self {
            quadlet_dir,
            generator_dir,
            control,
        }
    }

    /// Names of the units Quadlet generated, newest listing each time.
    ///
    /// An unreadable generator directory yields an empty list rather than an
    /// error: it does not exist before the first daemon-reload of a boot, and
    /// that is the normal state, not a fault.
    fn generated_units(&self) -> Vec<String> {
        let mut units = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.generator_dir) else {
            return units;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "service") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !body.contains(GENERATED_UNIT_MARKER) {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                units.push(name.to_string());
            }
        }
        units.sort();
        units
    }

    /// `.container`, `.kube` and `.pod` files the integrator left on STATE.
    ///
    /// Reported so that "containers are off" and "there was nothing to run
    /// anyway" are distinguishable in live state. They look identical from
    /// every unit-level observation.
    fn quadlet_files(&self) -> Vec<String> {
        let mut files = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.quadlet_dir) else {
            return files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_quadlet = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "container" | "kube" | "pod" | "volume" | "network"));
            if !is_quadlet {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                files.push(name.to_string());
            }
        }
        files.sort();
        files
    }

    /// Bring the bind up and re-run generators so Quadlet sees STATE.
    // Each step logs before it runs, not after. A reconciler that blocks leaves
    // no evidence otherwise: micad is Type=dbus and runs apply_all BEFORE it
    // acquires its bus name, so a hang here shows up as a unit stuck in
    // "activating" with nothing in the journal naming the step it stopped at.
    async fn turn_on(&self) -> Result<Vec<String>> {
        tracing::info!("container: turn_on begin");
        tracing::info!("container: reading unit_file_state");
        if !is_enabled(&self.control.unit_file_state(QUADLET_MOUNT_UNIT).await?) {
            self.control.enable(QUADLET_MOUNT_UNIT).await?;
        }
        tracing::info!("container: reading active_state");
        if !is_active(&self.control.active_state(QUADLET_MOUNT_UNIT).await?) {
            tracing::info!("container: starting the bind");
            self.control.start(QUADLET_MOUNT_UNIT).await?;
        }
        // After the mount, never before: a generator run with the directory
        // still unmounted parses the image's empty one and produces nothing,
        // and every step would have succeeded.
        tracing::info!("container: requesting daemon-reload");
        self.control.daemon_reload().await?;
        tracing::info!("container: daemon-reload returned");

        // And then start them. Without this step the switch does nothing at
        // all: a daemon-reload re-runs Quadlet, which writes the unit AND --
        // when the .container file has an [Install] section -- the .wants
        // symlink saying it should be running, but systemd does not act on a
        // symlink that appeared during a reload. It starts wanted units when
        // the target is started, and multi-user.target is reached long before
        // micad runs. The units then exist, are marked as wanted, and sit
        // inactive: the bind mounted, the reconciler reporting success, and no
        // container ever running.
        //
        // This is not orchestration. The integrator wrote `WantedBy=` and
        // Quadlet already acted on it; mos is making an instruction that was
        // given take effect, not deciding anything about what should run or in
        // what order.
        let mut started = Vec::new();
        let generated = self.generated_units();
        tracing::info!(count = generated.len(), units = ?generated, "container: units Quadlet generated");
        for unit in generated {
            if !is_active(&self.control.active_state(&unit).await?) {
                self.control.start(&unit).await?;
                started.push(unit);
            }
        }
        Ok(started)
    }

    /// Stop what is running, then take the bind down.
    ///
    /// Order matters and the reverse is a real failure: unmounting first makes
    /// the `.container` files invisible, the next daemon-reload removes the
    /// generated units from systemd's view, and the containers they started go
    /// on running as orphans that no unit name can now stop.
    async fn turn_off(&self) -> Result<Vec<String>> {
        let mut stopped = Vec::new();
        for unit in self.generated_units() {
            if is_active(&self.control.active_state(&unit).await?) {
                self.control.stop(&unit).await?;
                stopped.push(unit);
            }
        }
        if is_active(&self.control.active_state(QUADLET_MOUNT_UNIT).await?) {
            self.control.stop(QUADLET_MOUNT_UNIT).await?;
        }
        if is_enabled(&self.control.unit_file_state(QUADLET_MOUNT_UNIT).await?) {
            self.control.disable(QUADLET_MOUNT_UNIT).await?;
        }
        // Now the generated units may go: their source is gone, so this reload
        // is what removes them rather than what creates them.
        self.control.daemon_reload().await?;
        Ok(stopped)
    }
}

impl ContainerReconciler<Systemd> {
    /// Production reconciler: paths from [`QUADLET_DIR_ENV`] and
    /// [`GENERATOR_DIR_ENV`] if set, else the system locations.
    pub fn production() -> Self {
        let quadlet_dir = std::env::var(QUADLET_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_QUADLET_DIR));
        let generator_dir = std::env::var(GENERATOR_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_GENERATOR_DIR));
        Self::new(quadlet_dir, generator_dir, Systemd::new())
    }
}

#[async_trait::async_trait]
impl<C: UnitControl> Reconciler for ContainerReconciler<C> {
    fn name(&self) -> &'static str {
        "container"
    }

    fn subtree(&self) -> &'static str {
        "container"
    }

    async fn apply(&self, settings: &Settings) -> Result<serde_json::Value> {
        let enabled = settings.container.enabled;
        let (started, stopped) = if enabled {
            (self.turn_on().await?, Vec::new())
        } else {
            (Vec::new(), self.turn_off().await?)
        };

        // Read AFTER the transition, so what is published is what the system
        // now is rather than what it was asked to become.
        let mount_state = self.control.active_state(QUADLET_MOUNT_UNIT).await?;
        let units = if enabled {
            self.generated_units()
        } else {
            Vec::new()
        };
        let files = self.quadlet_files();

        Ok(serde_json::json!({
            "enabled": enabled,
            "quadletMountUnit": QUADLET_MOUNT_UNIT,
            "quadletMountState": mount_state,
            // Distinguishes "off" from "nothing to run": with the bind down the
            // list is empty because the directory is the image's, so it is
            // reported only when the bind is up.
            "quadletFiles": if enabled { files } else { Vec::new() },
            "generatedUnits": units,
            // Distinguishes "the switch is on and units exist" from "the switch
            // is on and something is actually running". They were the same
            // value while nothing was ever started.
            "startedUnits": started,
            "stoppedUnits": stopped,
        }))
    }
}

#[cfg(test)]
mod tests;
