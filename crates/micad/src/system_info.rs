//! Read-only system-information surface (PLAN-052 / RFCT-288): what this
//! device IS, assembled in one read from the seams that already exist.
//!
//! The shape [`crate::network_state`], [`crate::time_status`] and
//! [`crate::storage_status`] take: a trait with an unavailable default so a
//! dry-run daemon or a test never inspects its host, a production observer
//! only `main.rs` attaches, and a pure rendering function the tests drive
//! with literal evidence. Served as `GetSystemInfo` on the bus and
//! `GET /api/v1/system/info` over HTTPS.
//!
//! **Nothing here is a second mechanism.** Every fact is read from where it
//! already lives and is never restated anywhere else:
//!
//! - `/etc/machine-id`, seeded by systemd from the U-Boot environment
//!   (`rootfs/overlay/usr/lib/mica/mos-machine-id`).
//! - `/usr/share/mica/manifest.tsv`, the shipped bill of materials
//!   `rootfs/compose/90-pack.Dockerfile` writes before the package manager is
//!   purged: one row per installed package, `package\tversion\tarchitecture`,
//!   `#`-prefixed header. `verify/src/checks-root.ts` (`packed-mos-manifest`)
//!   holds an image to that shape and to the ONE `+git<commit>[.dirty]-<rev>`
//!   stamp every mos row shares; this module parses the same shape and
//!   reports the stamp it finds, consistent or not.
//! - `/usr/share/mica/release-identity.env`, which
//!   `rootfs/compose/compose-install.sh` writes from the arguments of the
//!   build that composed the image. Its `COMMIT_DATE` is the commit date of
//!   the commit the pool's `+git<commit>` stamp names, so it is a property of
//!   the SOURCE and not of the run: every reproducible build of one commit
//!   states the same instant, which a wall clock read at build time could not.
//! - The manifest file's mtime, which `rootfs/scripts/pack-squashfs.sh` pins
//!   to `SOURCE_DATE_EPOCH` with `-all-time`. It is NOT a build date and this
//!   module no longer reports it as one: `build/src/geometry.ts` pins that
//!   epoch to a constant so two builds of one tree are byte-identical, so the
//!   value is the same instant in every image this repository has ever
//!   produced. What it truthfully is -- the timestamp every file in the root
//!   carries -- is what it is reported as, under `system.fileEpoch`.
//! - `/proc/sys/kernel/{osrelease,version}`, what `uname -r` / `uname -v`
//!   print, read as files so a fixture tree can stand in for the host.
//! - `/etc/os-release`, the distribution's own identity file.
//! - The board model from the device tree (`/sys/firmware/devicetree/base/
//!   model`) or, on x64, DMI (`/sys/class/dmi/id/`).
//! - The running deployment, from the native deployment client the bus layer
//!   already holds; it is handed to [`info_json`] by the caller because the
//!   client is the bus layer's, not this module's.
//! - `/proc/uptime`.
//! - `/usr/share/mica/meta/`, the baked half of the `meta/` seam
//!   (`docs/plan/PLAN-070.md`): the update configuration `rootfs/build.sh`
//!   stages there, and the `GENERATED` marker it stages beside it exactly when
//!   the signing material the image was built from came from a generator
//!   rather than a ceremony. That is what lets a device answer "do I trust a
//!   development CA" without a build host to ask (`docs/plan/PLAN-077.md` §3).
//!
//! **Absence is data.** A file that is not there, a manifest row that does not
//! parse, a board with no device-tree model — each is reported as
//! `available: false` with a reason, never as an empty string that reads like
//! a value.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde_json::{Value as Json, json};

use crate::deployment::Status;

/// The shipped bill of materials, relative to the observer's root.
pub const MANIFEST_PATH: &str = "usr/share/mica/manifest.tsv";
/// systemd's machine id, relative to the observer's root.
pub const MACHINE_ID_PATH: &str = "etc/machine-id";
/// The distribution identity file, relative to the observer's root.
pub const OS_RELEASE_PATH: &str = "etc/os-release";
/// The device-tree model string (arm boards), relative to the root.
pub const DT_MODEL_PATH: &str = "sys/firmware/devicetree/base/model";
/// DMI's product name (x64 boards), relative to the root.
pub const DMI_PRODUCT_PATH: &str = "sys/class/dmi/id/product_name";
/// DMI's board vendor (x64 boards), relative to the root.
pub const DMI_VENDOR_PATH: &str = "sys/class/dmi/id/board_vendor";
/// The kernel release (`uname -r`), relative to the root.
pub const KERNEL_RELEASE_PATH: &str = "proc/sys/kernel/osrelease";
/// The kernel version string (`uname -v`), relative to the root.
pub const KERNEL_VERSION_PATH: &str = "proc/sys/kernel/version";
/// Seconds since boot, relative to the root.
pub const UPTIME_PATH: &str = "proc/uptime";
/// The device identity the composition writes, relative to the root.
pub const RELEASE_IDENTITY_PATH: &str = "usr/share/mica/release-identity.env";
/// The key in [`RELEASE_IDENTITY_PATH`] carrying the source commit's date,
/// as `rootfs/compose/compose-install.sh` spells it.
pub const COMMIT_DATE_KEY: &str = "COMMIT_DATE";
/// The baked update configuration — the mandatory member of the `meta/` public
/// set, and therefore the evidence that this image was built by a pipeline
/// that provisions a trust anchor at all. Relative to the observer's root.
pub const BAKED_META_MANIFEST_PATH: &str = "usr/share/mica/meta/updates/manifest.json";
/// The development-grade marker, baked beside it exactly when the signing
/// material this image was built from was generated by
/// `pkgs/mica-boot/dev-keys.sh`. Relative to the observer's root.
pub const BAKED_META_MARKER_PATH: &str = "usr/share/mica/meta/GENERATED";
/// The key in the marker naming the domains the generator wrote.
pub const MARKER_DOMAINS_KEY: &str = "DOMAINS";

/// The most manifest rows the surface carries. A real image ships a few
/// hundred; the cap exists so a corrupted or hostile file cannot grow the
/// answer without bound, and crossing it is reported as `truncated`.
pub const MAX_MANIFEST_ROWS: usize = 4096;

/// The packages whose manifest row names the system (image) version, in
/// order of preference: the system metapackage where the image has one, and
/// the management daemon otherwise. Both are mos rows and therefore carry the
/// pool's git stamp.
const SYSTEM_VERSION_PACKAGES: [&str; 2] = ["mica-system", "micad"];

/// One row of the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRow {
    /// Debian package name.
    pub name: String,
    /// Debian version string, which for mos rows ends in the git stamp.
    pub version: String,
    /// Debian architecture.
    pub architecture: String,
}

impl PackageRow {
    /// Whether this row is one of this repository's packages, by the rule
    /// `verify/src/checks-root.ts` applies: the name starts with `mos`.
    #[must_use]
    pub fn is_mos(&self) -> bool {
        self.name.starts_with("mica")
    }
}

/// The parsed manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The rows that parsed, in file order, at most [`MAX_MANIFEST_ROWS`].
    pub rows: Vec<PackageRow>,
    /// Rows that were not three tab-separated fields.
    pub malformed: usize,
    /// Whether rows beyond the cap were dropped.
    pub truncated: bool,
}

/// The `+git<commit>[.dirty]-<rev>` stamp at the end of a mos package version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStamp {
    /// The abbreviated commit, 12 lowercase hex characters.
    pub commit: String,
    /// Whether the pool was built from a dirty tree.
    pub dirty: bool,
    /// The Debian revision after the stamp.
    pub revision: String,
}

impl GitStamp {
    /// The stamp as it is spelled in the version string, `git<commit>[.dirty]-<rev>`.
    #[must_use]
    pub fn spelled(&self) -> String {
        let dirty = if self.dirty { ".dirty" } else { "" };
        format!("git{}{}-{}", self.commit, dirty, self.revision)
    }
}

/// Parse the manifest text: `#` lines and blank lines are skipped, every
/// other line is three tab-separated fields or is counted malformed.
#[must_use]
pub fn parse_manifest(text: &str) -> Manifest {
    let mut manifest = Manifest::default();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
            manifest.malformed += 1;
            continue;
        }
        if manifest.rows.len() >= MAX_MANIFEST_ROWS {
            manifest.truncated = true;
            continue;
        }
        manifest.rows.push(PackageRow {
            name: fields[0].to_string(),
            version: fields[1].to_string(),
            architecture: fields[2].to_string(),
        });
    }
    manifest
}

/// The git stamp at the end of `version`, or `None` when the version does not
/// end in one. The shape is `verify`'s: `+git` followed by twelve lowercase
/// hex characters, an optional `.dirty`, then `-<digits>`.
#[must_use]
pub fn parse_git_stamp(version: &str) -> Option<GitStamp> {
    let (_, stamp) = version.rsplit_once("+git")?;
    let (head, revision) = stamp.rsplit_once('-')?;
    if revision.is_empty() || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (commit, dirty) = match head.strip_suffix(".dirty") {
        Some(commit) => (commit, true),
        None => (head, false),
    };
    if commit.len() != 12
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(GitStamp {
        commit: commit.to_string(),
        dirty,
        revision: revision.to_string(),
    })
}

/// Parse `os-release`: `KEY=value` lines, values optionally double- or
/// single-quoted, comments and blank lines skipped. No escape processing
/// beyond stripping the quotes, which is all the fields this surface reports
/// ever need.
#[must_use]
pub fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            })
            .unwrap_or(value);
        fields.insert(key.trim().to_string(), value.to_string());
    }
    fields
}

/// Whether `text` is a machine id in the form systemd writes: 32 lowercase
/// hex characters.
#[must_use]
pub fn is_machine_id(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Where the board model was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    /// The model string, trimmed of the device tree's trailing NUL.
    pub model: String,
    /// `devicetree` or `dmi`.
    pub source: &'static str,
}

/// The grade of the signing material this image was built from.
///
/// Not guessed from the bytes of anything: `rootfs/build.sh` bakes
/// [`BAKED_META_MARKER_PATH`] if and only if the tree's `meta/GENERATED` is
/// there, and `verify`'s `packed-meta-is-the-public-set` refuses an image in
/// which those two disagree. So this is a fact the build recorded, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustGrade {
    /// The marker is baked: the CA in this image's keyring, or the package
    /// signing key it trusts, or both, came out of a generator rather than a
    /// ceremony. `domains` is what the marker's `DOMAINS=` line names, so a
    /// mixed image says WHICH half.
    Development { domains: Vec<String> },
    /// The baked set is there and carries no marker: material an operator
    /// placed on purpose.
    Production,
}

/// Everything the observer reads. Each field is what one soft read produced;
/// absent means the read did not answer.
#[derive(Debug, Clone)]
pub struct SystemInfoEvidence {
    /// A well-formed machine id, or the reason there is none.
    pub machine_id: Result<String, String>,
    /// The board model, when the firmware exports one.
    pub board: Option<Board>,
    /// `uname -r`.
    pub kernel_release: Option<String>,
    /// `uname -v`.
    pub kernel_version: Option<String>,
    /// The parsed `os-release`, empty when the file is absent.
    pub os_release: BTreeMap<String, String>,
    /// The parsed manifest, or `None` when the file is absent.
    pub manifest: Option<Manifest>,
    /// The manifest file's mtime in seconds since the epoch: the pinned
    /// `SOURCE_DATE_EPOCH` every file in the image carries, not a build date.
    pub file_epoch: Option<u64>,
    /// [`COMMIT_DATE_KEY`] as the release identity states it — the date of
    /// the commit the image's git stamp names.
    pub commit_date: Option<String>,
    /// Whole seconds since boot.
    pub uptime_seconds: Option<u64>,
    /// The grade of this image's signing material, or `None` when the baked
    /// `meta/` tree is not there to be asked. **`None` is not `Production`**:
    /// an image that provisions no anchor has no marker for the same reason a
    /// production image has none, and reporting the two the same way would
    /// have a device claiming a production CA it cannot see.
    pub trust: Option<TrustGrade>,
}

impl Default for SystemInfoEvidence {
    /// Nothing read yet: the machine id is absent with a reason, everything
    /// else is absent.
    fn default() -> Self {
        Self {
            machine_id: Err("not read".to_string()),
            board: None,
            kernel_release: None,
            kernel_version: None,
            os_release: BTreeMap::new(),
            manifest: None,
            file_epoch: None,
            commit_date: None,
            uptime_seconds: None,
            trust: None,
        }
    }
}

/// This daemon's own identity, as its build embedded it — the same two
/// values `micad --version` prints, read from the same place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonIdentity {
    /// `CARGO_PKG_NAME`.
    pub name: &'static str,
    /// `CARGO_PKG_VERSION`.
    pub version: &'static str,
    /// `MICA_BUILD_COMMIT`, when the build supplied one.
    pub commit: Option<&'static str>,
}

impl DaemonIdentity {
    /// The identity of this build.
    #[must_use]
    pub fn this_build() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            commit: option_env!("MICA_BUILD_COMMIT").filter(|commit| !commit.trim().is_empty()),
        }
    }
}

/// Read-only source of system-information evidence.
#[async_trait::async_trait]
pub trait SystemInfoSource: Send + Sync {
    /// Observe the current evidence.
    ///
    /// # Errors
    ///
    /// Returns an error only when this daemon has no observer at all; a
    /// production observer answers with absent evidence instead.
    async fn observe(&self) -> Result<SystemInfoEvidence>;
}

/// The observer a daemon without host access has: none.
pub struct UnavailableSystemInfo;

#[async_trait::async_trait]
impl SystemInfoSource for UnavailableSystemInfo {
    async fn observe(&self) -> Result<SystemInfoEvidence> {
        Err(anyhow::anyhow!(
            "this daemon observes no system information"
        ))
    }
}

/// Production observer over a filesystem root.
///
/// `root` is a path prefix rather than a hard-coded `/` so the whole read can
/// be driven over a fixture tree in a unit test; production is the same thing
/// rooted at `/`.
pub struct HostSystemInfo {
    root: PathBuf,
}

impl HostSystemInfo {
    /// The observer `main.rs` attaches on a device.
    #[must_use]
    pub fn production() -> Self {
        Self::at("/")
    }

    /// An observer rooted at `root`.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn read_trimmed(&self, relative: &str) -> Option<String> {
        let bytes = std::fs::read(self.path(relative)).ok()?;
        // The device tree's model string is NUL-terminated; everything else
        // is a text file with a trailing newline. Both are trimmed the same
        // way.
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!text.is_empty()).then(|| text.to_string())
    }

    fn machine_id(&self) -> Result<String, String> {
        let path = self.path(MACHINE_ID_PATH);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let text = text.trim();
                if is_machine_id(text) {
                    Ok(text.to_string())
                } else {
                    Err(format!(
                        "/{MACHINE_ID_PATH} is not a 32-character lowercase hex machine id"
                    ))
                }
            }
            Err(err) => Err(format!("/{MACHINE_ID_PATH} is not readable: {err}")),
        }
    }

    fn board(&self) -> Option<Board> {
        if let Some(model) = self.read_trimmed(DT_MODEL_PATH) {
            return Some(Board {
                model,
                source: "devicetree",
            });
        }
        let product = self.read_trimmed(DMI_PRODUCT_PATH)?;
        let model = match self.read_trimmed(DMI_VENDOR_PATH) {
            Some(vendor) => format!("{vendor} {product}"),
            None => product,
        };
        Some(Board {
            model,
            source: "dmi",
        })
    }

    fn manifest(&self) -> (Option<Manifest>, Option<u64>) {
        let path = self.path(MANIFEST_PATH);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (None, None);
        };
        let epoch = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_secs());
        (Some(parse_manifest(&text)), epoch)
    }

    /// [`COMMIT_DATE_KEY`] from the release identity, trimmed; `None` when
    /// the file is absent or states nothing for the key. That file is
    /// `KEY=VALUE`, the shape [`parse_os_release`] already reads, so it is
    /// parsed by the same function rather than by a second parser that could
    /// come to disagree with it. The value is reported as written: it is
    /// `git show -s --format=%cI`'s output, and `verify`'s
    /// `packed-release-identity` is what holds an image to that shape.
    fn commit_date(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.path(RELEASE_IDENTITY_PATH)).ok()?;
        let value = parse_os_release(&text).remove(COMMIT_DATE_KEY)?;
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    /// The grade of the signing material this image was built from.
    ///
    /// **The manifest is read first, and that is the whole vacuity guard.**
    /// "No marker" and "no baked `meta/` at all" are the same absent file and
    /// different facts: an image built before this seam, or one whose staging
    /// broke, has no marker *and* no anchor, and answering `Production` for it
    /// would be this surface saying the device trusts a production CA when it
    /// cannot see one. [`BAKED_META_MANIFEST_PATH`] is a required member of
    /// the public set, so its presence is the evidence that there is a tree
    /// here to be asked.
    fn trust(&self) -> Option<TrustGrade> {
        if !self.path(BAKED_META_MANIFEST_PATH).is_file() {
            return None;
        }
        let marker = self.path(BAKED_META_MARKER_PATH);
        if !marker.exists() {
            return Some(TrustGrade::Production);
        }
        // Present and unreadable still reports Development, with no domains.
        // The marker's presence is the claim; its contents only say which
        // half, and a read that failed must not promote the image to
        // production for want of a domain list.
        let text = std::fs::read_to_string(&marker).unwrap_or_default();
        Some(TrustGrade::Development {
            domains: marker_domains(&text),
        })
    }

    fn uptime_seconds(&self) -> Option<u64> {
        let contents = std::fs::read_to_string(self.path(UPTIME_PATH)).ok()?;
        let secs: f64 = contents.split_whitespace().next()?.parse().ok()?;
        (secs.is_finite() && secs >= 0.0).then_some(secs as u64)
    }
}

#[async_trait::async_trait]
impl SystemInfoSource for HostSystemInfo {
    async fn observe(&self) -> Result<SystemInfoEvidence> {
        let (manifest, file_epoch) = self.manifest();
        Ok(SystemInfoEvidence {
            machine_id: self.machine_id(),
            board: self.board(),
            kernel_release: self.read_trimmed(KERNEL_RELEASE_PATH),
            kernel_version: self.read_trimmed(KERNEL_VERSION_PATH),
            os_release: std::fs::read_to_string(self.path(OS_RELEASE_PATH))
                .map(|text| parse_os_release(&text))
                .unwrap_or_default(),
            manifest,
            file_epoch,
            commit_date: self.commit_date(),
            uptime_seconds: self.uptime_seconds(),
            trust: self.trust(),
        })
    }
}

/// Report a single current-domain declaration. An unreadable or malformed
/// declaration leaves the image development-grade without inferred domains.
fn marker_domains(text: &str) -> Vec<String> {
    let prefix = format!("{MARKER_DOMAINS_KEY}=");
    let mut declarations = text.lines().filter_map(|line| line.strip_prefix(&prefix));
    let Some(value) = declarations.next() else {
        return Vec::new();
    };
    if declarations.next().is_some() {
        return Vec::new();
    }
    let domains: Vec<_> = value.split_whitespace().collect();
    if domains
        .iter()
        .any(|domain| !matches!(*domain, "boot" | "verity" | "updates"))
        || domains
            .iter()
            .enumerate()
            .any(|(index, domain)| domains[..index].contains(domain))
    {
        return Vec::new();
    }
    domains.into_iter().map(str::to_string).collect()
}

/// `{"available": false, "detail": detail}` — the one spelling of absence.
fn absent(detail: impl Into<String>) -> Json {
    json!({ "available": false, "detail": detail.into() })
}

/// RFC 3339 for `epoch`, or `None` when it is out of range.
fn rfc3339(epoch: u64) -> Option<String> {
    let secs = i64::try_from(epoch).ok()?;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|when| when.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// `evidence` rendered as the JSON the bus method serves.
///
/// Every member is an object carrying `available`, so a consumer reads one
/// shape whether the fact is there or not, and an absent fact carries the
/// reason it is absent.
#[must_use]
pub fn info_json(
    evidence: &SystemInfoEvidence,
    deployment: Option<&Status>,
    daemon: &DaemonIdentity,
) -> Json {
    let machine_id = match &evidence.machine_id {
        Ok(id) => json!({ "available": true, "id": id }),
        Err(detail) => absent(detail.clone()),
    };
    let board = match &evidence.board {
        Some(board) => json!({ "available": true, "model": board.model, "source": board.source }),
        None => absent("no device-tree model and no DMI product name is exported by this board"),
    };
    let kernel = match (&evidence.kernel_release, &evidence.kernel_version) {
        (None, None) => absent(format!("/{KERNEL_RELEASE_PATH} is not readable")),
        (release, version) => json!({
            "available": true,
            "release": release,
            "version": version,
        }),
    };
    let release = if evidence.os_release.is_empty() {
        absent(format!("/{OS_RELEASE_PATH} is absent or empty"))
    } else {
        let mut root = serde_json::Map::new();
        root.insert("available".to_string(), json!(true));
        for (key, member) in [
            ("NAME", "name"),
            ("ID", "id"),
            ("VERSION", "version"),
            ("VERSION_ID", "versionId"),
            ("PRETTY_NAME", "prettyName"),
            ("BUILD_ID", "buildId"),
            ("IMAGE_ID", "imageId"),
            ("IMAGE_VERSION", "imageVersion"),
        ] {
            if let Some(value) = evidence.os_release.get(key) {
                root.insert(member.to_string(), json!(value));
            }
        }
        Json::Object(root)
    };
    let (system, packages) = match &evidence.manifest {
        None => (
            absent(format!(
                "/{MANIFEST_PATH} is absent; this root was not packed by the image pipeline"
            )),
            absent(format!("/{MANIFEST_PATH} is absent")),
        ),
        Some(manifest) => (
            system_json(
                manifest,
                evidence.file_epoch,
                evidence.commit_date.as_deref(),
            ),
            packages_json(manifest),
        ),
    };
    let deployment = match deployment
        .and_then(|status| status.booted().map(|booted| (status, booted)))
    {
        None => absent("the native deployment backend did not report a running deployment"),
        Some((status, booted)) => json!({"available":true,"id":booted.id,"version":booted.version,
            "generation":booted.generation,"kernelId":booted.kernel_id,"kernelRelease":booted.kernel_release,
            "rootfsId":booted.rootfs_id,"confirmed":status.state.current.as_ref() == Some(&booted.id),
            "contentVerified":status.boot.content_verified,"secureBoot":status.boot.secure_boot,
            "backend":status.boot.backend,"bootVerified":status.boot.boot_verified}),
    };
    let uptime = match evidence.uptime_seconds {
        Some(seconds) => json!({ "available": true, "seconds": seconds }),
        None => absent(format!("/{UPTIME_PATH} is not readable")),
    };
    let trust = match &evidence.trust {
        None => absent(format!(
            "/{BAKED_META_MANIFEST_PATH} is absent, so this image provisions no trust anchor \
             and the grade of the material it was built from cannot be read off it -- which is \
             not the same as production"
        )),
        Some(TrustGrade::Production) => json!({ "available": true, "grade": "production" }),
        Some(TrustGrade::Development { domains }) => json!({
            "available": true,
            "grade": "development",
            "developmentDomains": domains,
            "marker": format!("/{BAKED_META_MARKER_PATH}"),
        }),
    };
    json!({
        "machineId": machine_id,
        "board": board,
        "kernel": kernel,
        "release": release,
        "system": system,
        "trust": trust,
        "daemon": {
            "name": daemon.name,
            "version": daemon.version,
            "commit": daemon.commit,
        },
        "packages": packages,
        "deployment": deployment,
        "uptime": uptime,
    })
}

/// The `system` member: the image version by way of the system package's
/// manifest row, the git stamp the mos rows share, the date of the commit
/// that stamp names, and the pinned file epoch the root carries.
///
/// The two times are DIFFERENT FACTS and are named apart on purpose.
/// `commitDate` is when the source was committed, so it moves with the tree
/// and is the same for every rebuild of one commit. `fileEpoch` is the
/// `SOURCE_DATE_EPOCH` `build/src/geometry.ts` pins to a constant, which is
/// why every file in every mos image carries it; naming it `buildDate` — as
/// this surface once did — reported 2020-01-01 as the day each image was
/// made, on every image ever built.
fn system_json(manifest: &Manifest, file_epoch: Option<u64>, commit_date: Option<&str>) -> Json {
    let version_row = SYSTEM_VERSION_PACKAGES
        .iter()
        .find_map(|name| manifest.rows.iter().find(|row| row.name == *name));
    let mut stamps: Vec<String> = manifest
        .rows
        .iter()
        .filter(|row| row.is_mos())
        .map(|row| {
            parse_git_stamp(&row.version)
                .map_or_else(|| "unstamped".to_string(), |stamp| stamp.spelled())
        })
        .collect();
    stamps.sort();
    stamps.dedup();
    let consistent = stamps.len() == 1 && stamps[0] != "unstamped";
    let stamp = version_row.and_then(|row| parse_git_stamp(&row.version));
    let git_stamp = match &stamp {
        Some(stamp) => json!({
            "available": true,
            "commit": stamp.commit,
            "dirty": stamp.dirty,
            "revision": stamp.revision,
            "consistent": consistent,
            "stamps": stamps,
        }),
        None => json!({
            "available": false,
            "detail": "the system package's version carries no +git stamp",
            "consistent": consistent,
            "stamps": stamps,
        }),
    };
    let mut root = serde_json::Map::new();
    match version_row {
        Some(row) => {
            root.insert("available".to_string(), json!(true));
            root.insert("version".to_string(), json!(row.version));
            root.insert("package".to_string(), json!(row.name));
        }
        None => {
            root.insert("available".to_string(), json!(false));
            root.insert(
                "detail".to_string(),
                json!(format!(
                    "no {} row in the manifest",
                    SYSTEM_VERSION_PACKAGES.join(" or ")
                )),
            );
        }
    }
    root.insert("gitStamp".to_string(), git_stamp);
    root.insert(
        "commitDate".to_string(),
        commit_date_json(commit_date, stamp.as_ref()),
    );
    root.insert("fileEpoch".to_string(), file_epoch_json(file_epoch));
    Json::Object(root)
}

/// The `system.commitDate` member: when the commit the image's stamp names
/// was committed, as `rootfs/compose/compose-install.sh` recorded it.
///
/// A `.dirty` stamp is stated rather than hidden. It means the pool was built
/// from a tree that no commit reproduces, so the date below is that commit's
/// and the source that was packaged was not exactly it — which a bare date
/// would invite a reader to assume it was.
fn commit_date_json(commit_date: Option<&str>, stamp: Option<&GitStamp>) -> Json {
    let Some(date) = commit_date else {
        return absent(format!(
            "/{RELEASE_IDENTITY_PATH} states no {COMMIT_DATE_KEY}; it is written by \
             rootfs/compose/compose-install.sh from the commit the pool's git stamp names, so a \
             root without it was composed by something else or before the field existed"
        ));
    };
    let mut node = serde_json::Map::new();
    node.insert("available".to_string(), json!(true));
    node.insert("date".to_string(), json!(date));
    if let Some(stamp) = stamp
        && stamp.dirty
    {
        node.insert(
            "detail".to_string(),
            json!(format!(
                "the pool this image was composed from carries a .dirty stamp, so this is when \
                 commit {} was committed and the tree that was packaged was not exactly it",
                stamp.commit
            )),
        );
    }
    Json::Object(node)
}

/// The `system.fileEpoch` member: the mtime every file in this root carries.
///
/// Not a build date and deliberately not named as one. `rootfs/scripts/
/// pack-squashfs.sh` pins every time in the image to `SOURCE_DATE_EPOCH`, and
/// `build/src/geometry.ts` pins that to a constant so two builds of one tree
/// are byte-identical — so this value is a property of the repository's
/// reproducibility rule, not of when anything was built. It is reported
/// because it IS what the filesystem says, and a reader comparing a file's
/// timestamp against this surface should find the two agreeing.
fn file_epoch_json(file_epoch: Option<u64>) -> Json {
    let Some(epoch) = file_epoch else {
        return absent(format!(
            "/{MANIFEST_PATH} has no readable mtime, so the epoch every file in this root was \
             pinned to cannot be read back from it"
        ));
    };
    let mut node = serde_json::Map::new();
    node.insert("available".to_string(), json!(true));
    node.insert("epoch".to_string(), json!(epoch));
    if let Some(date) = rfc3339(epoch) {
        node.insert("date".to_string(), json!(date));
    }
    Json::Object(node)
}

/// The `packages` member: every manifest row, with the mos ones marked.
fn packages_json(manifest: &Manifest) -> Json {
    let entries: Vec<Json> = manifest
        .rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "version": row.version,
                "architecture": row.architecture,
                "mos": row.is_mos(),
            })
        })
        .collect();
    json!({
        "available": true,
        "count": manifest.rows.len(),
        "mosCount": manifest.rows.iter().filter(|row| row.is_mos()).count(),
        "malformedRows": manifest.malformed,
        "truncated": manifest.truncated,
        "entries": entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "#package\tversion\tarchitecture\n\
        base-files\t13.8\tarm64\n\
        mica-apid\t0.1.0+git00b674ec0ffe-1\tarm64\n\
        mica-deploy\t0.1.0+git00b674ec0ffe-1\tarm64\n\
        micad\t0.1.0+git00b674ec0ffe-1\tarm64\n\
        systemd\t257.7-1\tarm64\n";

    fn daemon() -> DaemonIdentity {
        DaemonIdentity {
            name: "micad",
            version: "0.1.0",
            commit: Some("00b674ec0ffe"),
        }
    }

    /// A fixture tree carrying every seam, so the assembly — the part where
    /// this module's bugs would live — runs end to end without a device.
    fn fixture_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let write = |relative: &str, contents: &[u8]| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write fixture");
        };
        write(MACHINE_ID_PATH, b"0123456789abcdef0123456789abcdef\n");
        write(MANIFEST_PATH, MANIFEST.as_bytes());
        write(
            OS_RELEASE_PATH,
            b"PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"13\"\nID=debian\n# a comment\n",
        );
        write(DT_MODEL_PATH, b"Vendor CX3576 Board\0");
        write(KERNEL_RELEASE_PATH, b"6.1.115-mos\n");
        write(
            KERNEL_VERSION_PATH,
            b"#1 SMP PREEMPT Mon Sep 1 00:00:00 UTC 2026\n",
        );
        write(UPTIME_PATH, b"12345.67 8888.00\n");
        write(
            RELEASE_IDENTITY_PATH,
            b"# What this device is, for native deployment status.\nBOARD=cx3576\nPROFILE=dev\n\
              VERSION=0.1.0+git00b674ec0ffe-1\nCOMMIT_DATE=2026-09-01T12:34:56+08:00\n",
        );
        // PRODUCTION-shaped: the baked public set's required member is there
        // and no marker is beside it, because that is the released state. A
        // test that wants the development branch writes the marker itself.
        write(
            BAKED_META_MANIFEST_PATH,
            b"{ \"schema\": \"mos/meta/v1\", \"update\": { \"source\": null } }\n",
        );
        dir
    }

    #[test]
    fn the_manifest_parses_by_the_shipped_shape() {
        let manifest = parse_manifest(MANIFEST);
        assert_eq!(manifest.rows.len(), 5);
        assert_eq!(manifest.malformed, 0);
        assert!(!manifest.truncated);
        assert_eq!(manifest.rows[1].name, "mica-apid");
        assert_eq!(manifest.rows[1].version, "0.1.0+git00b674ec0ffe-1");
        assert_eq!(manifest.rows[1].architecture, "arm64");
        assert!(manifest.rows[1].is_mos());
        assert!(!manifest.rows[0].is_mos());
    }

    /// A row that is not three fields is counted, not silently dropped and
    /// not allowed to poison the rows around it.
    #[test]
    fn a_malformed_row_is_counted_and_skipped() {
        let manifest = parse_manifest("a\t1\n\nb\t2\tamd64\tx\nc\t3\tamd64\n#h\n\t\t\n");
        assert_eq!(manifest.rows.len(), 1);
        assert_eq!(manifest.rows[0].name, "c");
        assert_eq!(manifest.malformed, 3);
    }

    /// The cap is enforced, and crossing it is reported rather than hidden.
    #[test]
    fn rows_beyond_the_cap_are_dropped_and_flagged() {
        let text: String = (0..(MAX_MANIFEST_ROWS + 5))
            .map(|i| format!("p{i}\t1\tamd64\n"))
            .collect();
        let manifest = parse_manifest(&text);
        assert_eq!(manifest.rows.len(), MAX_MANIFEST_ROWS);
        assert!(manifest.truncated);
    }

    #[test]
    fn the_git_stamp_is_the_verify_shape_and_nothing_else() {
        let stamp = parse_git_stamp("0.1.0+git00b674ec0ffe-1").expect("a clean stamp");
        assert_eq!(stamp.commit, "00b674ec0ffe");
        assert!(!stamp.dirty);
        assert_eq!(stamp.revision, "1");
        assert_eq!(stamp.spelled(), "git00b674ec0ffe-1");

        let dirty = parse_git_stamp("1.13+git00b674ec0ffe.dirty-3").expect("a dirty stamp");
        assert!(dirty.dirty);
        assert_eq!(dirty.spelled(), "git00b674ec0ffe.dirty-3");

        for shapeless in [
            "257.7-1",
            "1.0+git00b674ec0ff-1",
            "1.0+gitZZb674ec0ffe-1",
            "1.0+git00b674ec0ffe",
            "1.0+git00b674ec0ffe-x",
            "1.0+git00b674ec0ffe-",
        ] {
            assert_eq!(parse_git_stamp(shapeless), None, "{shapeless}");
        }
    }

    #[test]
    fn os_release_values_lose_their_quotes() {
        let fields = parse_os_release("A=\"x y\"\nB='z'\nC=plain\n# c\n\nD\n");
        assert_eq!(fields["A"], "x y");
        assert_eq!(fields["B"], "z");
        assert_eq!(fields["C"], "plain");
        assert!(!fields.contains_key("D"));
    }

    #[test]
    fn a_machine_id_is_thirty_two_lowercase_hex() {
        assert!(is_machine_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_machine_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_machine_id("0123456789abcdef0123456789abcde"));
        assert!(!is_machine_id(""));
    }

    /// The assembly over a full fixture tree: every seam read from where it
    /// lives, the commit date read from the release identity, and the pinned
    /// file epoch read from the manifest's mtime.
    #[tokio::test]
    async fn a_full_root_yields_every_member_available() {
        let root = fixture_root();
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        let deployment = Status::parse(&crate::deployment::tests::fixture().to_string()).unwrap();
        let info = info_json(&evidence, Some(&deployment), &daemon());

        assert_eq!(info["machineId"]["available"], true);
        assert_eq!(info["machineId"]["id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(info["board"]["model"], "Vendor CX3576 Board");
        assert_eq!(info["board"]["source"], "devicetree");
        assert_eq!(info["kernel"]["release"], "6.1.115-mos");
        assert!(
            info["kernel"]["version"]
                .as_str()
                .is_some_and(|v| v.starts_with("#1 SMP"))
        );
        assert_eq!(info["release"]["id"], "debian");
        assert_eq!(
            info["release"]["prettyName"],
            "Debian GNU/Linux 13 (trixie)"
        );
        assert_eq!(info["release"]["versionId"], "13");
        assert_eq!(info["system"]["available"], true);
        assert_eq!(info["system"]["package"], "micad");
        assert_eq!(info["system"]["version"], "0.1.0+git00b674ec0ffe-1");
        assert_eq!(info["system"]["gitStamp"]["commit"], "00b674ec0ffe");
        assert_eq!(info["system"]["gitStamp"]["dirty"], false);
        assert_eq!(info["system"]["gitStamp"]["consistent"], true);
        assert_eq!(
            info["system"]["gitStamp"]["stamps"],
            json!(["git00b674ec0ffe-1"])
        );
        // The commit date is the release identity's, verbatim -- a fact about
        // the source, not about this run and not about any file's mtime.
        assert_eq!(info["system"]["commitDate"]["available"], true);
        assert_eq!(
            info["system"]["commitDate"]["date"],
            "2026-09-01T12:34:56+08:00"
        );
        // A clean stamp carries no caveat.
        assert!(info["system"]["commitDate"]["detail"].is_null());
        // The file epoch is the manifest's mtime, which the fixture wrote just
        // now: a real epoch, rendered RFC 3339 beside it. On a composed image
        // it is the pinned SOURCE_DATE_EPOCH instead, which is why it is not
        // the commit date and is not named like one.
        let epoch = info["system"]["fileEpoch"]["epoch"]
            .as_u64()
            .expect("file epoch");
        assert!(epoch > 1_700_000_000, "epoch {epoch}");
        assert!(
            info["system"]["fileEpoch"]["date"]
                .as_str()
                .is_some_and(|d| d.ends_with('Z')),
            "{}",
            info["system"]
        );
        assert_eq!(info["daemon"]["name"], "micad");
        assert_eq!(info["daemon"]["commit"], "00b674ec0ffe");
        assert_eq!(info["packages"]["count"], 5);
        assert_eq!(info["packages"]["mosCount"], 3);
        assert_eq!(info["packages"]["entries"][3]["name"], "micad");
        assert_eq!(info["packages"]["entries"][3]["mos"], true);
        assert_eq!(info["deployment"]["id"], "a".repeat(64));
        assert_eq!(info["deployment"]["kernelId"], "c".repeat(64));
        assert_eq!(info["deployment"]["rootfsId"], "d".repeat(64));
        assert_eq!(info["uptime"]["seconds"], 12345);
    }

    /// The empty root: every member is present and says WHY it is absent.
    /// Nothing is manufactured, and nothing panics.
    #[tokio::test]
    async fn an_empty_root_reports_every_member_absent_with_a_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence = HostSystemInfo::at(dir.path())
            .observe()
            .await
            .expect("observe");
        let info = info_json(&evidence, None, &daemon());
        for member in [
            "machineId",
            "board",
            "kernel",
            "release",
            "system",
            "packages",
            "deployment",
            "uptime",
        ] {
            assert_eq!(
                info[member]["available"], false,
                "{member}: {}",
                info[member]
            );
            assert!(
                info[member]["detail"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "{member} carries no reason: {}",
                info[member]
            );
        }
        // The daemon's own identity needs no file and is always there.
        assert_eq!(info["daemon"]["version"], "0.1.0");
    }

    /// A machine id that is not the systemd shape is absent evidence, not a
    /// value that looks like one.
    #[tokio::test]
    async fn a_malformed_machine_id_is_absent_with_the_reason() {
        let root = fixture_root();
        std::fs::write(root.path().join(MACHINE_ID_PATH), "not-an-id\n").expect("write");
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        let info = info_json(&evidence, None, &daemon());
        assert_eq!(info["machineId"]["available"], false);
        assert!(
            info["machineId"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("32-character")),
            "{}",
            info["machineId"]
        );
    }

    /// x64 has no device tree: the board comes from DMI, vendor first.
    #[tokio::test]
    async fn a_dmi_board_is_read_when_there_is_no_device_tree() {
        let root = fixture_root();
        std::fs::remove_file(root.path().join(DT_MODEL_PATH)).expect("drop dt model");
        let write = |relative: &str, contents: &str| {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        };
        write(DMI_PRODUCT_PATH, "NUC13ANHi5\n");
        write(DMI_VENDOR_PATH, "Intel Corporation\n");
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        let info = info_json(&evidence, None, &daemon());
        assert_eq!(info["board"]["model"], "Intel Corporation NUC13ANHi5");
        assert_eq!(info["board"]["source"], "dmi");
    }

    /// Two stamps in one manifest is a half-rebuilt pool, and the surface
    /// says so rather than picking one.
    #[test]
    fn inconsistent_stamps_are_reported_not_hidden() {
        let manifest = parse_manifest(
            "mica-apid\t0.1.0+git00b674ec0ffe-1\tarm64\n\
             micad\t0.1.0+gitffffffffffff.dirty-1\tarm64\n",
        );
        let system = system_json(&manifest, None, Some("2026-09-01T12:34:56+08:00"));
        assert_eq!(system["gitStamp"]["consistent"], false);
        assert_eq!(
            system["gitStamp"]["stamps"],
            json!(["git00b674ec0ffe-1", "gitffffffffffff.dirty-1"])
        );
        assert_eq!(system["gitStamp"]["dirty"], true);
        assert_eq!(system["fileEpoch"]["available"], false);
        assert!(
            system["fileEpoch"]["detail"]
                .as_str()
                .is_some_and(|d| !d.is_empty())
        );
    }

    /// A `.dirty` stamp means the packaged tree is not the commit the date
    /// belongs to, and the field SAYS so instead of reading like a plain
    /// timestamp of the source that shipped.
    #[test]
    fn a_dirty_stamp_is_stated_beside_the_commit_date() {
        let manifest = parse_manifest("micad\t0.1.0+gitffffffffffff.dirty-1\tarm64\n");
        let system = system_json(&manifest, Some(1_577_836_800), Some("2026-09-01T00:00:00Z"));
        assert_eq!(system["commitDate"]["available"], true);
        assert_eq!(system["commitDate"]["date"], "2026-09-01T00:00:00Z");
        assert!(
            system["commitDate"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("ffffffffffff") && d.contains(".dirty")),
            "{}",
            system["commitDate"]
        );
    }

    /// A root whose identity states no commit date says so. It must not fall
    /// back to the pinned file epoch -- which is a real value, is present in
    /// the same member, and would read as a build date that is 2020-01-01 on
    /// every image this repository has ever produced.
    #[tokio::test]
    async fn an_identity_without_a_commit_date_is_absent_and_never_the_file_epoch() {
        let root = fixture_root();
        std::fs::write(
            root.path().join(RELEASE_IDENTITY_PATH),
            "BOARD=cx3576\nPROFILE=dev\nVERSION=0.1.0+git00b674ec0ffe-1\n",
        )
        .expect("write");
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        assert_eq!(evidence.commit_date, None);
        let info = info_json(&evidence, None, &daemon());
        assert_eq!(info["system"]["commitDate"]["available"], false);
        assert!(
            info["system"]["commitDate"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains(COMMIT_DATE_KEY)),
            "{}",
            info["system"]["commitDate"]
        );
        assert!(info["system"]["commitDate"]["date"].is_null());
        // The file epoch is still reported, under its own name.
        assert_eq!(info["system"]["fileEpoch"]["available"], true);
    }

    /// An identity stating the key with nothing after it is absence, not an
    /// empty string that renders as a blank date.
    #[tokio::test]
    async fn an_empty_commit_date_is_absence() {
        let root = fixture_root();
        std::fs::write(
            root.path().join(RELEASE_IDENTITY_PATH),
            "BOARD=cx3576\nCOMMIT_DATE=\n",
        )
        .expect("write");
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        assert_eq!(evidence.commit_date, None);
    }

    /// The unavailable default refuses rather than inspecting the host.
    #[tokio::test]
    async fn the_unavailable_source_is_an_error() {
        assert!(UnavailableSystemInfo.observe().await.is_err());
    }

    /// The released state: the baked set is there and no marker is beside it.
    #[tokio::test]
    async fn a_root_with_no_marker_beside_its_baked_manifest_is_production() {
        let root = fixture_root();
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        assert_eq!(evidence.trust, Some(TrustGrade::Production));
        let info = info_json(&evidence, None, &daemon());
        assert_eq!(info["trust"]["available"], true);
        assert_eq!(info["trust"]["grade"], "production");
        // No marker path and no domain list on a production image: they are
        // facts about a marker, and there is none.
        assert!(info["trust"]["marker"].is_null());
        assert!(info["trust"]["developmentDomains"].is_null());
    }

    /// The marker is baked, so the device says so — and names WHICH half,
    /// because the mixed tree is real: a content signing ceremony's output
    /// copied in while the package signing key is still development-grade.
    #[tokio::test]
    async fn a_baked_marker_reports_development_and_the_domains_it_names() {
        let root = fixture_root();
        std::fs::write(
            root.path().join(BAKED_META_MARKER_PATH),
            "DEVELOPMENT-GRADE\nDOMAINS=boot verity updates\n",
        )
        .expect("write");
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        assert_eq!(
            evidence.trust,
            Some(TrustGrade::Development {
                domains: vec![
                    "boot".to_string(),
                    "verity".to_string(),
                    "updates".to_string()
                ],
            })
        );
        let info = info_json(&evidence, None, &daemon());
        assert_eq!(info["trust"]["grade"], "development");
        assert_eq!(info["trust"]["developmentDomains"][2], "updates");
        assert_eq!(
            info["trust"]["marker"],
            format!("/{BAKED_META_MARKER_PATH}")
        );
    }

    /// **The vacuity case, and the one this member exists to get right.** No
    /// baked `meta/` tree at all — an image built before the seam, or one
    /// whose staging broke — has no marker for the same reason a production
    /// image has none. Reporting it as production would have the surface
    /// claim a production CA on a device that provisions no anchor, so it is
    /// reported ABSENT with the reason.
    #[tokio::test]
    async fn a_root_with_no_baked_meta_at_all_is_absent_and_never_production() {
        let root = fixture_root();
        std::fs::remove_file(root.path().join(BAKED_META_MANIFEST_PATH)).expect("rm");
        let evidence = HostSystemInfo::at(root.path())
            .observe()
            .await
            .expect("observe");
        assert_eq!(evidence.trust, None);
        let info = info_json(&evidence, None, &daemon());
        assert_eq!(info["trust"]["available"], false);
        assert_ne!(info["trust"]["grade"], "production");
        assert!(
            info["trust"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains(BAKED_META_MANIFEST_PATH)),
            "{}",
            info["trust"]
        );
    }

    /// A marker with no `DOMAINS=` line is still a marker. The presence is the
    /// claim; the line only says which half.
    #[test]
    fn a_marker_naming_no_domain_is_still_development() {
        assert_eq!(marker_domains("no domains here\n"), Vec::<String>::new());
        assert_eq!(marker_domains("DOMAINS=unknown\n"), Vec::<String>::new());
        assert_eq!(marker_domains("DOMAINS=boot boot\n"), Vec::<String>::new());
        // A duplicate assignment is not an alternate key-generation format.
        assert_eq!(
            marker_domains("DOMAINS=boot\nDOMAINS=boot verity updates\n"),
            Vec::<String>::new()
        );
    }
}
