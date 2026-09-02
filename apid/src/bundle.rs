//! The `/mos/ui` bundle store: layout, validation, atomic activation,
//! deactivate, explicit delete, and the "what is installed right now?" read.
//!
//! This is `docs/design/api.md` §5.2 and §5.3 as a self-contained module. It
//! contains no HTTP: the asset router (§4) and the start-up wiring (§6.1) are
//! separate work and call in here.
//!
//! Layout, all of it under one root (`/mos/ui` on a device, a temporary
//! directory in tests):
//!
//! ```text
//! <root>/bundles/<generation>/   installed trees
//! <root>/current                 symlink to the active bundles/<generation>
//! <root>/records/<generation>.json   digest and compatibility result
//! <root>/.staging-<generation>/  a tree waiting to be validated
//! <root>/.trash-<generation>/    a tree being deleted
//! ```
//!
//! **Absence of the root is a defined state, not an error** — it is the
//! shipped state of every device and §6.1's first failure class. The root is
//! created lazily on first activation and never at start-up.

// There is no module-level `allow(dead_code)`: an unreachable entry point here
// is a warning. The `allow`s below are per-item and carry their reason, so a
// gap is visible in the source rather than absorbed by a file-wide suppression.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{
    DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink,
};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The shipped location of the bundle store (§5.2).
pub const DEFAULT_ROOT: &str = "/mos/ui";

/// The answer the status read gives when no custom bundle is active. §5.3
/// requires this state to be a named answer, never an empty field.
pub const NO_CUSTOM_BUNDLE: &str = "no custom bundle is active; the built-in UI is being served";

/// The optional manifest at the root of a bundle tree (§5.3).
const MANIFEST_NAME: &str = "mos-ui.json";
/// The one file every bundle must have at its root (§5.3, §6.1 class 2).
const INDEX_NAME: &str = "index.html";
/// Mode for the root and every directory beneath it (§5.2).
const DIR_MODE: u32 = 0o755;
/// Mode for every file in the store (§5.2).
const FILE_MODE: u32 = 0o644;

/// `mos-ui.json`. A bundle without one is valid — see [`CompatCheck::NotRun`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    /// Human-readable bundle name.
    pub name: String,
    /// Bundle version, opaque to apid.
    pub version: String,
    /// Directory, relative to the bundle root, whose contents may be served
    /// `Cache-Control: immutable` (§4.3). Serving it is §4's work, not this
    /// module's; the store only carries the declaration.
    #[serde(rename = "immutableDir")]
    pub immutable_dir: String,
    /// The API versions this bundle was built against. Compared with §2.1's
    /// served set by **intersection**, never by equality with `current`
    /// (§6.1 class 5).
    #[serde(rename = "apiVersions")]
    pub api_versions: Vec<String>,
}

/// Whether §6.1 class 5's compatibility check ran, and what it decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum CompatCheck {
    /// The bundle carries no manifest, so nothing could be checked. §5.3
    /// requires such a bundle to be activated and recorded as unchecked.
    NotRun,
    /// The check ran against a served set.
    Ran {
        /// The manifest's declared API versions.
        declared: Vec<String>,
        /// The served set the declaration was compared against (§2.1).
        served: Vec<String>,
        /// True when the intersection is non-empty.
        compatible: bool,
    },
}

/// What activation recorded about a generation, on disk under `records/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    /// Digest of the tree as it was activated.
    digest: String,
    /// The compatibility check as it stood at activation.
    compat: CompatCheck,
    /// Whole uploaded ZIP size; absent for legacy/manual installs.
    #[serde(skip_serializing_if = "Option::is_none")]
    compressed_bytes: Option<u64>,
    /// Expanded package size; absent for legacy/manual installs.
    #[serde(skip_serializing_if = "Option::is_none")]
    expanded_bytes: Option<u64>,
}

/// The outcome of a successful activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// The generation now pointed at by `current`.
    pub generation: u64,
    /// The digest recorded for it.
    pub digest: String,
    /// The compatibility check recorded for it.
    pub compat: CompatCheck,
    /// The manifest, when the bundle carries one.
    pub manifest: Option<Manifest>,
}

/// A refusal with a reason. Every variant leaves the active bundle untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// `.staging-<generation>` is missing or is not a directory.
    StagingNotDirectory(PathBuf),
    /// `bundles/<generation>` already exists; activation never overwrites one.
    GenerationExists(u64),
    /// No `index.html` at the root of the tree (§6.1 class 2).
    MissingIndex,
    /// `index.html` exists but is not a regular file (§6.1 class 2).
    IndexNotRegularFile,
    /// `index.html` is a regular file that cannot be opened.
    IndexUnreadable(String),
    /// An entry that is neither a regular file nor a directory (§5.3 rule 2,
    /// §4.4 rule 5's primary mitigation).
    IrregularEntry {
        /// Path relative to the bundle root.
        path: PathBuf,
        /// What it was instead.
        kind: EntryKind,
    },
    /// A regular file with more than one link — a hardlink, which §5.3 rule 2
    /// rejects for the same reason as a symlink: it names bytes outside the
    /// tree the operator staged.
    Hardlink {
        /// Path relative to the bundle root.
        path: PathBuf,
        /// The link count found.
        links: u64,
    },
    /// `mos-ui.json` is present but does not parse.
    ManifestUnparsable(String),
    /// The declared range and the served set have no member in common — the
    /// one and only trigger in §6.1 class 5.
    Incompatible {
        /// The manifest's declared API versions.
        declared: Vec<String>,
        /// The served set compared against.
        served: Vec<String>,
    },
    /// Delete was called on the generation `current` points at. Deactivate
    /// first: §5.3 never unlinks the tree `current` resolves to.
    DeleteWhileActive(u64),
    /// The archive is byte-for-byte the same tree as an installed package.
    DuplicatePackage(u64),
    /// A name/version pair already identifies different installed bytes.
    NameVersionConflict {
        name: String,
        version: String,
        generation: u64,
    },
    /// Uploads never delete older versions implicitly.
    RetentionLimit(usize),
}

/// What an offending entry turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A symbolic link.
    Symlink,
    /// A FIFO.
    Fifo,
    /// A unix socket.
    Socket,
    /// A block device node.
    BlockDevice,
    /// A character device node.
    CharDevice,
    /// Something the kernel reported that is none of the above.
    Unknown,
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Symlink => "symlink",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
            Self::BlockDevice => "block device",
            Self::CharDevice => "character device",
            Self::Unknown => "not a regular file or directory",
        };
        f.write_str(name)
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StagingNotDirectory(path) => {
                write!(f, "{} is not a staged directory", path.display())
            }
            Self::GenerationExists(generation) => {
                write!(f, "bundle generation {generation} already exists")
            }
            Self::MissingIndex => write!(f, "no {INDEX_NAME} at the root of the bundle"),
            Self::IndexNotRegularFile => write!(f, "{INDEX_NAME} is not a regular file"),
            Self::IndexUnreadable(why) => write!(f, "{INDEX_NAME} is unreadable: {why}"),
            Self::IrregularEntry { path, kind } => {
                write!(
                    f,
                    "{} is a {kind}; a bundle carries only regular files and directories",
                    path.display()
                )
            }
            Self::Hardlink { path, links } => {
                write!(
                    f,
                    "{} is a hardlink ({links} links); a bundle carries only single-linked regular files",
                    path.display()
                )
            }
            Self::ManifestUnparsable(why) => write!(f, "{MANIFEST_NAME} does not parse: {why}"),
            Self::Incompatible { declared, served } => write!(
                f,
                "declared API versions [{}] have no member in common with the served set [{}]",
                declared.join(", "),
                served.join(", ")
            ),
            Self::DeleteWhileActive(generation) => write!(
                f,
                "generation {generation} is the active bundle; deactivate before deleting"
            ),
            Self::DuplicatePackage(generation) => write!(
                f,
                "the same package is already installed as generation {generation}"
            ),
            Self::NameVersionConflict {
                name,
                version,
                generation,
            } => write!(
                f,
                "{name} {version} is already generation {generation} with different content"
            ),
            Self::RetentionLimit(limit) => write!(
                f,
                "the store already retains {limit} UI packages; delete one before uploading"
            ),
        }
    }
}

impl std::error::Error for Rejection {}

/// A bundle name/version pair, for the status read.
// §5.3 status read: complete and tested, no route exposes it yet (§8.2).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSummary {
    /// `name` from the manifest.
    pub name: String,
    /// `version` from the manifest.
    pub version: String,
}

/// What activation recorded, re-evaluated against the tree as it is now.
// §5.3 status read: complete and tested, no route exposes it yet (§8.2).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedState {
    /// The digest recorded at activation.
    pub digest: String,
    /// Whether the tree still hashes to it. A tree that cannot be hashed at
    /// all — an operator planted a symlink in it over a root shell — counts
    /// as a mismatch, because it is one.
    pub digest_matches: bool,
    /// The compatibility check as recorded at activation.
    pub compat: CompatCheck,
}

/// The active bundle, read from the served tree.
// §5.3 status read: complete and tested, no route exposes it yet (§8.2).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomUi {
    /// The generation `current` resolves to.
    pub generation: u64,
    /// The manifest read from the tree **now**, or `None` for "no manifest".
    pub manifest: Option<ManifestSummary>,
    /// Whether `index.html` exists and opens **right now**.
    pub index_readable: bool,
    /// What activation recorded, or `None` when this generation has no
    /// activation record — a tree placed under `bundles/` by hand.
    pub recorded: Option<RecordedState>,
}

/// Why an installed custom UI cannot currently be selected.
///
/// These are deliberately stable, path-free states suitable for an API
/// response. Detailed I/O failures stay in apid's logs rather than exposing
/// filesystem layout to a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateUnavailable {
    /// The installed tree has no valid activation record.
    MissingActivationRecord,
    /// The tree contains an irregular entry or could not be walked safely.
    UnsafeTree,
    /// The root `index.html` is missing, irregular or unreadable.
    IndexUnavailable,
    /// The optional manifest is present but cannot be read or parsed.
    ManifestInvalid,
    /// The installed tree no longer matches its activation digest.
    DigestMismatch,
    /// The manifest does not support an API version served by this apid.
    Incompatible,
}

/// One retained generation inspected against the API versions served now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCandidate {
    /// The installed generation.
    pub generation: u64,
    /// Name and version from a valid manifest, when one exists.
    pub manifest: Option<ManifestSummary>,
    /// Whether the root `index.html` is a readable regular file.
    pub index_readable: bool,
    /// Whether the current tree matches the activation record, when one was
    /// available and the tree could be hashed.
    pub digest_matches: Option<bool>,
    /// Whether the manifest intersects the currently served API set. `None`
    /// means the bundle has no manifest and is therefore unchecked.
    pub compatible: Option<bool>,
    /// SHA-256 recorded when the generation was installed.
    pub digest: Option<String>,
    /// Whole uploaded ZIP size, absent for legacy/manual generations.
    pub compressed_bytes: Option<u64>,
    /// Expanded package size, absent for legacy/manual generations.
    pub expanded_bytes: Option<u64>,
    /// True only when the generation can safely be selected now.
    pub usable: bool,
    /// The first failed safety check, absent when [`Self::usable`] is true.
    pub unavailable_reason: Option<CandidateUnavailable>,
}

/// The result of selecting a retained custom UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The fully revalidated candidate selected by the operation.
    pub candidate: CustomCandidate,
    /// False when `current` already pointed at this generation.
    pub changed: bool,
}

/// The answer to "what is installed right now?" (§5.3).
// §5.3 status read: complete and tested, no route exposes it yet (§8.2).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// No custom bundle is active; the built-in UI is being served. This is a
    /// named answer, not an absent one.
    BuiltIn,
    /// A custom bundle is active.
    Custom(CustomUi),
}

impl Installed {
    /// The one-line statement of the state, so no caller has to invent the
    /// wording for the no-bundle case.
    // §5.3 status read: complete and tested, no route exposes it yet (§8.2).
    #[allow(dead_code)]
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::BuiltIn => NO_CUSTOM_BUNDLE.to_string(),
            Self::Custom(ui) => match &ui.manifest {
                Some(m) => format!(
                    "generation {} is active: {} {}",
                    ui.generation, m.name, m.version
                ),
                None => format!("generation {} is active: no manifest", ui.generation),
            },
        }
    }
}

/// The result of re-evaluating the active bundle (§6.1 classes 3 and 5).
///
/// Every field is a state, never an error: §6.1 forbids a bundle being able to
/// fail apid's start-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recheck {
    /// The active generation.
    pub generation: u64,
    /// Whether the tree still matches its recorded digest; `None` when no
    /// digest was recorded for this generation.
    pub digest_matches: Option<bool>,
    /// The compatibility check re-run against the served set passed in.
    pub compat: CompatCheck,
}

impl Recheck {
    /// True only when the declared range and the served set have an empty
    /// intersection — §6.1's sole deactivation trigger. A bundle with no
    /// manifest is never incompatible, because nothing was checked.
    // No caller: `startup::discover` reaches the same verdict by matching
    // `CompatCheck` directly. Kept as the named predicate beside `corrupt`,
    // which IS called, so §6.1's two triggers stay readable as a pair.
    #[allow(dead_code)]
    #[must_use]
    pub fn incompatible(&self) -> bool {
        matches!(
            self.compat,
            CompatCheck::Ran {
                compatible: false,
                ..
            }
        )
    }

    /// True when the tree no longer matches the digest recorded at activation
    /// (§6.1 class 3).
    #[must_use]
    pub fn corrupt(&self) -> bool {
        self.digest_matches == Some(false)
    }
}

/// The bundle store rooted at a directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// A store rooted anywhere. Tests root it in a temporary directory; the
    /// device uses [`Store::at_default`].
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store at `/mos/ui` (§5.2).
    #[must_use]
    pub fn at_default() -> Self {
        Self::new(DEFAULT_ROOT)
    }

    /// The root this store manages.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where an operator places a tree before calling [`Store::activate`].
    #[must_use]
    pub fn staging_dir(&self, generation: u64) -> PathBuf {
        self.root.join(format!(".staging-{generation}"))
    }

    /// The installed tree for a generation.
    #[must_use]
    pub fn bundle_dir(&self, generation: u64) -> PathBuf {
        self.bundles_dir().join(generation.to_string())
    }

    fn bundles_dir(&self) -> PathBuf {
        self.root.join("bundles")
    }

    fn records_dir(&self) -> PathBuf {
        self.root.join("records")
    }

    fn record_path(&self, generation: u64) -> PathBuf {
        self.records_dir().join(format!("{generation}.json"))
    }

    fn trash_dir(&self, generation: u64) -> PathBuf {
        self.root.join(format!(".trash-{generation}"))
    }

    /// The active pointer (§5.3 step 4).
    #[must_use]
    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    /// The generation `current` points at, or `None` when no bundle is active.
    ///
    /// Reads the link rather than following it, so a `current` left dangling
    /// by a hand-deleted tree still names its generation.
    pub fn active_generation(&self) -> anyhow::Result<Option<u64>> {
        let link = self.current_link();
        let target = match fs::read_link(&link) {
            Ok(target) => target,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("read link {}", link.display()));
            }
        };
        Ok(target
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u64>().ok()))
    }

    /// Generations with an installed tree, ascending.
    pub fn generations(&self) -> anyhow::Result<Vec<u64>> {
        numbered_children(&self.bundles_dir())
    }

    /// Every retained generation, newest first, revalidated for display.
    pub fn candidates(&self, served: &[&str]) -> anyhow::Result<Vec<CustomCandidate>> {
        self.generations()?
            .into_iter()
            .rev()
            .map(|generation| self.inspect_generation(generation, served))
            .collect()
    }

    /// The next monotonically increasing generation under the selection lock.
    pub fn next_generation(&self) -> anyhow::Result<u64> {
        self.generations()?
            .into_iter()
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .context("UI generation overflow")
    }

    /// Inspect retained generations and return the newest usable candidate.
    ///
    /// If no generation is usable, the newest installed generation is still
    /// returned with a named reason so the recovery UI can explain why its
    /// custom choice is disabled. A missing store is `Ok(None)` and this read
    /// never creates or repairs anything.
    pub fn available_custom(&self, served: &[&str]) -> anyhow::Result<Option<CustomCandidate>> {
        let mut newest_unusable = None;
        for generation in self.generations()?.into_iter().rev() {
            let candidate = self.inspect_generation(generation, served)?;
            if candidate.usable {
                return Ok(Some(candidate));
            }
            if newest_unusable.is_none() {
                newest_unusable = Some(candidate);
            }
        }
        Ok(newest_unusable)
    }

    /// Revalidate and select the newest usable retained generation.
    ///
    /// The server chooses the generation; callers cannot provide a path or a
    /// generation number. `Ok(None)` means installed trees are absent or all
    /// failed a safety check and leaves `current` untouched.
    #[cfg(test)]
    pub fn select_available_custom(&self, served: &[&str]) -> anyhow::Result<Option<Selection>> {
        let Some(candidate) = self.available_custom(served)? else {
            return Ok(None);
        };
        if !candidate.usable {
            return Ok(None);
        }
        let changed = !self.current_points_at(candidate.generation)?;
        if changed {
            self.point_current_at(candidate.generation)?;
        }
        Ok(Some(Selection { candidate, changed }))
    }

    /// Revalidate and select one explicit retained generation.
    pub fn select_generation(
        &self,
        generation: u64,
        served: &[&str],
    ) -> anyhow::Result<Option<Selection>> {
        if !self.generations()?.contains(&generation) {
            return Ok(None);
        }
        let candidate = self.inspect_generation(generation, served)?;
        if !candidate.usable {
            return Ok(None);
        }
        let changed = !self.current_points_at(generation)?;
        if changed {
            self.point_current_at(generation)?;
        }
        Ok(Some(Selection { candidate, changed }))
    }

    fn current_points_at(&self, generation: u64) -> anyhow::Result<bool> {
        let link = self.current_link();
        match fs::read_link(&link) {
            Ok(target) => {
                let expected = format!("bundles/{generation}");
                Ok(target.as_os_str() == std::ffi::OsStr::new(&expected))
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(false)
            }
            Err(err) => Err(anyhow::Error::new(err))
                .with_context(|| format!("read link {}", link.display())),
        }
    }

    fn inspect_generation(
        &self,
        generation: u64,
        served: &[&str],
    ) -> anyhow::Result<CustomCandidate> {
        let dir = self.bundle_dir(generation);
        let index = dir.join(INDEX_NAME);
        let index_readable = fs::symlink_metadata(&index).is_ok_and(|meta| meta.is_file())
            && File::open(&index).is_ok();
        let unavailable = |reason| CustomCandidate {
            generation,
            manifest: None,
            index_readable,
            digest_matches: None,
            compatible: None,
            digest: None,
            compressed_bytes: None,
            expanded_bytes: None,
            usable: false,
            unavailable_reason: Some(reason),
        };

        let entries = match validate_tree(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                let reason = match err.downcast_ref::<Rejection>() {
                    Some(
                        Rejection::MissingIndex
                        | Rejection::IndexNotRegularFile
                        | Rejection::IndexUnreadable(_),
                    ) => CandidateUnavailable::IndexUnavailable,
                    _ => CandidateUnavailable::UnsafeTree,
                };
                return Ok(unavailable(reason));
            }
        };
        let manifest = match read_manifest(&dir) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(unavailable(CandidateUnavailable::ManifestInvalid)),
        };
        let manifest_summary = manifest.as_ref().map(|manifest| ManifestSummary {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
        });
        let Some(record) = self.read_record(generation)? else {
            let mut candidate = unavailable(CandidateUnavailable::MissingActivationRecord);
            candidate.manifest = manifest_summary;
            return Ok(candidate);
        };
        let digest_matches =
            digest_tree(&dir, &entries).is_ok_and(|digest| digest == record.digest);
        if !digest_matches {
            return Ok(CustomCandidate {
                generation,
                manifest: manifest_summary,
                index_readable,
                digest_matches: Some(false),
                compatible: None,
                digest: Some(record.digest),
                compressed_bytes: record.compressed_bytes,
                expanded_bytes: record.expanded_bytes,
                usable: false,
                unavailable_reason: Some(CandidateUnavailable::DigestMismatch),
            });
        }
        let compatible = match check_compat(manifest.as_ref(), served) {
            Ok(CompatCheck::NotRun) => None,
            Ok(CompatCheck::Ran { compatible, .. }) => Some(compatible),
            Err(err)
                if matches!(
                    err.downcast_ref::<Rejection>(),
                    Some(Rejection::Incompatible { .. })
                ) =>
            {
                return Ok(CustomCandidate {
                    generation,
                    manifest: manifest_summary,
                    index_readable,
                    digest_matches: Some(true),
                    compatible: Some(false),
                    digest: Some(record.digest),
                    compressed_bytes: record.compressed_bytes,
                    expanded_bytes: record.expanded_bytes,
                    usable: false,
                    unavailable_reason: Some(CandidateUnavailable::Incompatible),
                });
            }
            Err(err) => return Err(err),
        };
        Ok(CustomCandidate {
            generation,
            manifest: manifest_summary,
            index_readable,
            digest_matches: Some(true),
            compatible,
            digest: Some(record.digest),
            compressed_bytes: record.compressed_bytes,
            expanded_bytes: record.expanded_bytes,
            usable: true,
            unavailable_reason: None,
        })
    }

    /// Generations with a staged tree waiting to be activated, ascending.
    pub fn discover_staged(&self) -> anyhow::Result<Vec<u64>> {
        let mut found = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(found),
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("read dir {}", self.root.display()));
            }
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(generation) = name
                .to_str()
                .and_then(|name| name.strip_prefix(".staging-"))
                .and_then(|digits| digits.parse::<u64>().ok())
            else {
                continue;
            };
            found.push(generation);
        }
        found.sort_unstable();
        Ok(found)
    }

    /// §8.2 phase 4's second sanctioned local install path: apid picks up a
    /// staged directory. Activates the highest staged generation and returns
    /// `None` when there is nothing staged.
    ///
    /// `served` is §2.1's served set. It is an input because §2 is not
    /// implemented: the set belongs to `GET /api/versions` and this module
    /// must not invent it.
    pub fn pick_up_staged(&self, served: &[&str]) -> anyhow::Result<Option<Activation>> {
        let Some(generation) = self.discover_staged()?.pop() else {
            return Ok(None);
        };
        self.activate(generation, served).map(Some)
    }

    /// Install `.staging-<generation>` without changing the active pointer.
    ///
    /// Validation runs on the staged tree and never on the live one, so every
    /// refusal leaves the previously active bundle active and untouched.
    ///
    /// # Errors
    ///
    /// Returns a downcastable [`Rejection`] when the staged tree is refused,
    /// and an I/O error otherwise. In both cases nothing an operator can see
    /// has changed.
    fn install_staged(
        &self,
        generation: u64,
        served: &[&str],
        enforce_upload_policy: bool,
        package_sizes: Option<(u64, u64)>,
    ) -> anyhow::Result<Activation> {
        let staging = self.staging_dir(generation);

        // Step 1 is the operator's or the upload path's: the tree is already
        // at `staging`, which is under the root and therefore on the same
        // filesystem as the target, so step 3's rename is atomic.
        let staged_meta = fs::symlink_metadata(&staging)
            .with_context(|| format!("stat {}", staging.display()))
            .map_err(|_| Rejection::StagingNotDirectory(staging.clone()))?;
        if !staged_meta.is_dir() {
            bail!(Rejection::StagingNotDirectory(staging));
        }
        let target = self.bundle_dir(generation);
        if fs::symlink_metadata(&target).is_ok() {
            bail!(Rejection::GenerationExists(generation));
        }
        let generations = self.generations()?;
        if enforce_upload_policy && generations.len() >= 32 {
            bail!(Rejection::RetentionLimit(32));
        }

        // Step 2: validate the staged tree, completely.
        let entries = validate_tree(&staging)?;
        let manifest = read_manifest(&staging)?;
        let compat = check_compat(manifest.as_ref(), served)?;

        apply_modes(&staging, &entries)?;
        let digest = digest_tree(&staging, &entries)?;

        for installed in generations {
            let installed_dir = self.bundle_dir(installed);
            let identity_conflict = if enforce_upload_policy {
                match (manifest.as_ref(), read_manifest(&installed_dir)) {
                    (Some(new), Ok(Some(old))) => {
                        new.name == old.name && new.version == old.version
                    }
                    _ => false,
                }
            } else {
                false
            };
            let installed_entries = match validate_tree(&installed_dir) {
                Ok(entries) => entries,
                Err(_) if !identity_conflict => continue,
                Err(_) => {
                    let new = manifest.as_ref().context("missing upload manifest")?;
                    bail!(Rejection::NameVersionConflict {
                        name: new.name.clone(),
                        version: new.version.clone(),
                        generation: installed,
                    });
                }
            };
            let installed_digest = match digest_tree(&installed_dir, &installed_entries) {
                Ok(digest) => digest,
                Err(_) if !identity_conflict => continue,
                Err(_) => {
                    let new = manifest.as_ref().context("missing upload manifest")?;
                    bail!(Rejection::NameVersionConflict {
                        name: new.name.clone(),
                        version: new.version.clone(),
                        generation: installed,
                    });
                }
            };
            if enforce_upload_policy && installed_digest == digest {
                bail!(Rejection::DuplicatePackage(installed));
            }
            if identity_conflict {
                let new = manifest.as_ref().context("missing upload manifest")?;
                bail!(Rejection::NameVersionConflict {
                    name: new.name.clone(),
                    version: new.version.clone(),
                    generation: installed,
                });
            }
        }

        // Step 3: fsync the staged tree and its parent, then rename.
        fsync_tree(&staging, &entries)?;
        self.ensure_layout()?;
        fsync_dir(&self.root)?;
        fs::rename(&staging, &target)
            .with_context(|| format!("rename {} to {}", staging.display(), target.display()))?;
        fsync_dir(&self.bundles_dir())?;
        self.write_record(
            generation,
            &Record {
                digest: digest.clone(),
                compat: compat.clone(),
                compressed_bytes: package_sizes.map(|sizes| sizes.0),
                expanded_bytes: package_sizes.map(|sizes| sizes.1),
            },
        )?;

        Ok(Activation {
            generation,
            digest,
            compat,
            manifest,
        })
    }

    /// Install an uploaded staged tree without changing the active pointer.
    pub fn install(
        &self,
        generation: u64,
        served: &[&str],
        compressed_bytes: u64,
        expanded_bytes: u64,
    ) -> anyhow::Result<Activation> {
        self.install_staged(
            generation,
            served,
            true,
            Some((compressed_bytes, expanded_bytes)),
        )
    }

    /// Install and activate a manually staged tree.
    pub fn activate(&self, generation: u64, served: &[&str]) -> anyhow::Result<Activation> {
        let activation = self.install_staged(generation, served, false, None)?;
        self.point_current_at(generation)?;
        Ok(activation)
    }

    /// §5.3's deactivate, which is also §6.3's escape: remove `current`. The
    /// bundle stays on disk. Returns whether a pointer was there to remove.
    pub fn deactivate(&self) -> anyhow::Result<bool> {
        let link = self.current_link();
        match fs::remove_file(&link) {
            Ok(()) => {
                fsync_dir(&self.root)?;
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => {
                Err(anyhow::Error::new(err)).with_context(|| format!("remove {}", link.display()))
            }
        }
    }

    /// §5.3's delete: rename the tree to `.trash-<generation>`, then unlink it
    /// recursively.
    ///
    /// # Errors
    ///
    /// Refuses with [`Rejection::DeleteWhileActive`] when `current` points at
    /// this generation. Deactivate first.
    pub fn delete(&self, generation: u64) -> anyhow::Result<()> {
        if self.active_generation()? == Some(generation) {
            bail!(Rejection::DeleteWhileActive(generation));
        }
        let dir = self.bundle_dir(generation);
        if fs::symlink_metadata(&dir).is_err() {
            return Ok(());
        }
        let trash = self.trash_dir(generation);
        if fs::symlink_metadata(&trash).is_ok() {
            fs::remove_dir_all(&trash).with_context(|| format!("remove {}", trash.display()))?;
        }
        fs::rename(&dir, &trash)
            .with_context(|| format!("rename {} to {}", dir.display(), trash.display()))?;
        fs::remove_dir_all(&trash).with_context(|| format!("remove {}", trash.display()))?;
        let record = self.record_path(generation);
        match fs::remove_file(&record) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("remove {}", record.display()));
            }
        }
        Ok(())
    }

    /// "What is installed right now?" (§5.3), answered from the served tree.
    ///
    /// An absent root, an absent `current` and an unresolvable `current` are
    /// all [`Installed::BuiltIn`] — a named answer, never an error and never
    /// an empty field.
    // §5.3 status read: complete and tested, no route exposes it yet (§8.2).
    #[allow(dead_code)]
    pub fn status(&self) -> anyhow::Result<Installed> {
        let Some(generation) = self.active_generation()? else {
            return Ok(Installed::BuiltIn);
        };
        let dir = self.bundle_dir(generation);
        let manifest = read_manifest(&dir).ok().flatten().map(|m| ManifestSummary {
            name: m.name,
            version: m.version,
        });
        let index = dir.join(INDEX_NAME);
        let index_readable =
            fs::symlink_metadata(&index).is_ok_and(|m| m.is_file()) && File::open(&index).is_ok();
        let recorded = self.read_record(generation)?.map(|record| RecordedState {
            digest_matches: digest_of(&dir).is_ok_and(|digest| digest == record.digest),
            digest: record.digest,
            compat: record.compat,
        });
        Ok(Installed::Custom(CustomUi {
            generation,
            manifest,
            index_readable,
            recorded,
        }))
    }

    /// Re-evaluate the active bundle against a served set: §6.1 class 3's
    /// digest re-check and class 5's compatibility re-check, which run at
    /// activation and at apid start-up and **not** per request.
    ///
    /// Returns `None` when no bundle is active. Never deactivates anything —
    /// the caller owns that decision and the log line §6.1 requires.
    pub fn recheck_active(&self, served: &[&str]) -> anyhow::Result<Option<Recheck>> {
        let Some(generation) = self.active_generation()? else {
            return Ok(None);
        };
        let dir = self.bundle_dir(generation);
        let digest_matches = self
            .read_record(generation)?
            .map(|record| digest_of(&dir).is_ok_and(|digest| digest == record.digest));
        let manifest = read_manifest(&dir).ok().flatten();
        let compat = match check_compat(manifest.as_ref(), served) {
            Ok(compat) => compat,
            Err(err) => match err.downcast_ref::<Rejection>() {
                Some(Rejection::Incompatible { declared, served }) => CompatCheck::Ran {
                    declared: declared.clone(),
                    served: served.clone(),
                    compatible: false,
                },
                _ => return Err(err),
            },
        };
        Ok(Some(Recheck {
            generation,
            digest_matches,
            compat,
        }))
    }

    /// Create the root, `bundles/` and `records/` with mode 0755. Called from
    /// the install path only: §5.2 requires that nothing create `/mos/ui` at
    /// start-up.
    fn ensure_layout(&self) -> anyhow::Result<()> {
        for dir in [self.root.clone(), self.bundles_dir(), self.records_dir()] {
            if !dir.exists() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(DIR_MODE)
                    .create(&dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
        }
        Ok(())
    }

    fn point_current_at(&self, generation: u64) -> anyhow::Result<()> {
        let staged_link = self.root.join(format!(".current-{generation}.tmp"));
        match fs::remove_file(&staged_link) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("remove {}", staged_link.display()));
            }
        }
        // A relative target, so the pointer stays correct however the root is
        // reached.
        symlink(format!("bundles/{generation}"), &staged_link)
            .with_context(|| format!("symlink {}", staged_link.display()))?;
        let link = self.current_link();
        fs::rename(&staged_link, &link)
            .with_context(|| format!("rename {} to {}", staged_link.display(), link.display()))?;
        fsync_dir(&self.root)
    }

    fn write_record(&self, generation: u64, record: &Record) -> anyhow::Result<()> {
        let path = self.record_path(generation);
        let json = serde_json::to_vec_pretty(record).context("serialise activation record")?;
        write_file(&path, &json)?;
        fsync_dir(&self.records_dir())
    }

    fn read_record(&self, generation: u64) -> anyhow::Result<Option<Record>> {
        let path = self.record_path(generation);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => {
                Err(anyhow::Error::new(err)).with_context(|| format!("read {}", path.display()))
            }
        }
    }
}

/// One entry of a staged or installed tree, relative to its root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    is_dir: bool,
}

/// Walk `root`, rejecting every entry that is not a regular file or a
/// directory — §5.3 rule 2, enforced on the staged tree before anything is
/// reachable, which is §4.4 rule 5's primary mitigation.
fn collect(root: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    collect_into(root, Path::new(""), &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn collect_into(root: &Path, rel: &Path, out: &mut Vec<Entry>) -> anyhow::Result<()> {
    let dir = root.join(rel);
    for entry in fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let child_rel = rel.join(entry.file_name());
        let meta = entry
            .metadata()
            .with_context(|| format!("stat {}", root.join(&child_rel).display()))?;
        let file_type = meta.file_type();
        if file_type.is_dir() {
            out.push(Entry {
                path: child_rel.clone(),
                is_dir: true,
            });
            collect_into(root, &child_rel, out)?;
        } else if file_type.is_file() {
            if meta.nlink() > 1 {
                bail!(Rejection::Hardlink {
                    path: child_rel,
                    links: meta.nlink(),
                });
            }
            out.push(Entry {
                path: child_rel,
                is_dir: false,
            });
        } else {
            bail!(Rejection::IrregularEntry {
                path: child_rel,
                kind: classify(&file_type),
            });
        }
    }
    Ok(())
}

fn classify(file_type: &fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_fifo() {
        EntryKind::Fifo
    } else if file_type.is_socket() {
        EntryKind::Socket
    } else if file_type.is_block_device() {
        EntryKind::BlockDevice
    } else if file_type.is_char_device() {
        EntryKind::CharDevice
    } else {
        EntryKind::Unknown
    }
}

/// §5.3 step 2 on a staged tree: exactly one readable `index.html` at the
/// root, and no entry that is not a regular file or a directory.
fn validate_tree(root: &Path) -> anyhow::Result<Vec<Entry>> {
    let entries = collect(root)?;
    let index = root.join(INDEX_NAME);
    let meta = match fs::symlink_metadata(&index) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(Rejection::MissingIndex),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("stat {}", index.display()));
        }
    };
    if !meta.is_file() {
        bail!(Rejection::IndexNotRegularFile);
    }
    if let Err(err) = File::open(&index) {
        bail!(Rejection::IndexUnreadable(err.to_string()));
    }
    Ok(entries)
}

/// Read `mos-ui.json` from the root of a tree. Absent is `None` and valid;
/// present-but-unparsable is a rejection (§5.3 step 2).
fn read_manifest(root: &Path) -> anyhow::Result<Option<Manifest>> {
    let path = root.join(MANIFEST_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("read {}", path.display()));
        }
    };
    match serde_json::from_slice::<Manifest>(&bytes) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(err) => bail!(Rejection::ManifestUnparsable(err.to_string())),
    }
}

/// §6.1 class 5: **membership in the served set**, never equality with its
/// `current` member. The trigger is an empty intersection and nothing else, so
/// a bundle declaring only the outgoing major stays compatible for exactly the
/// generation §2.1 recommends serving both.
///
/// A bundle with no manifest cannot be checked, and is activated with the
/// check recorded as not run.
fn check_compat(manifest: Option<&Manifest>, served: &[&str]) -> anyhow::Result<CompatCheck> {
    let Some(manifest) = manifest else {
        return Ok(CompatCheck::NotRun);
    };
    let declared = manifest.api_versions.clone();
    let served: Vec<String> = served.iter().map(|v| (*v).to_string()).collect();
    let compatible = declared.iter().any(|v| served.contains(v));
    if !compatible {
        bail!(Rejection::Incompatible { declared, served });
    }
    Ok(CompatCheck::Ran {
        declared,
        served,
        compatible,
    })
}

/// §5.2's modes on the tree the store is about to install: 0755 on
/// directories, 0644 on files.
fn apply_modes(root: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    set_mode(root, DIR_MODE)?;
    for entry in entries {
        let path = root.join(&entry.path);
        set_mode(&path, if entry.is_dir { DIR_MODE } else { FILE_MODE })?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// Hash a tree: every entry's relative path and every file's length and
/// content, in sorted order, so the digest is a property of the tree rather
/// than of the order the filesystem happened to return it in.
fn digest_tree(root: &Path, entries: &[Entry]) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    for entry in entries {
        let path = root.join(&entry.path);
        if entry.is_dir {
            hasher.update(b"d\0");
            hasher.update(entry.path.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
        } else {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            hasher.update(b"f\0");
            hasher.update(entry.path.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
    }
    Ok(hex(&hasher.finalize()))
}

/// Hash an installed tree, walking it first. A tree that no longer walks —
/// something planted a symlink in it — is an error here, and every caller
/// turns that into "the digest does not match", because it does not.
fn digest_of(root: &Path) -> anyhow::Result<String> {
    let entries = collect(root)?;
    digest_tree(root, &entries)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// §5.3 step 3's fsync: without it a power cut can leave `current` resolving
/// to a tree whose data never reached the disk, which is failure class 3.
fn fsync_tree(root: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    for entry in entries {
        let path = root.join(&entry.path);
        if entry.is_dir {
            fsync_dir(&path)?;
        } else {
            File::open(&path)
                .with_context(|| format!("open {}", path.display()))?
                .sync_all()
                .with_context(|| format!("fsync {}", path.display()))?;
        }
    }
    fsync_dir(root)
}

fn fsync_dir(path: &Path) -> anyhow::Result<()> {
    File::open(path)
        .with_context(|| format!("open dir {}", path.display()))?
        .sync_all()
        .with_context(|| format!("fsync dir {}", path.display()))
}

fn write_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(path)
        .with_context(|| format!("open {} for writing", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", path.display()))
}

/// Numeric child directory names of `dir`, ascending. A missing `dir` is an
/// empty list, not an error.
fn numbered_children(dir: &Path) -> anyhow::Result<Vec<u64>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("read dir {}", dir.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(generation) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        found.push(generation);
    }
    found.sort_unstable();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::process::Command;

    /// The served set §2.1 recommends while two majors are served. Passed in
    /// rather than looked up: §2 is not implemented and this module must not
    /// invent the set.
    const SERVED: [&str; 2] = ["v1", "v2"];

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path().join("ui"));
        (dir, store)
    }

    /// Stage a minimal valid tree: one `index.html` and one asset.
    fn stage_valid(store: &Store, generation: u64) -> PathBuf {
        let staging = store.staging_dir(generation);
        fs::create_dir_all(staging.join("assets")).expect("create staging");
        fs::write(staging.join(INDEX_NAME), b"<!doctype html>").expect("write index");
        fs::write(staging.join("assets/app.js"), b"console.log(1)").expect("write asset");
        staging
    }

    fn write_manifest(staging: &Path, api_versions: &[&str]) {
        let manifest = serde_json::json!({
            "name": "demo",
            "version": "1.2.3",
            "immutableDir": "assets",
            "apiVersions": api_versions,
        });
        fs::write(
            staging.join(MANIFEST_NAME),
            serde_json::to_vec(&manifest).expect("serialise manifest"),
        )
        .expect("write manifest");
    }

    fn rejection(err: &anyhow::Error) -> Rejection {
        err.downcast_ref::<Rejection>()
            .unwrap_or_else(|| panic!("expected a Rejection, got: {err:#}"))
            .clone()
    }

    fn custom(installed: &Installed) -> &CustomUi {
        match installed {
            Installed::Custom(ui) => ui,
            Installed::BuiltIn => panic!("expected an active bundle"),
        }
    }

    /// §5.2: absence of the root is a defined state, and §5.3 requires the
    /// answer to be named rather than empty.
    #[test]
    fn absent_root_reads_as_the_built_in_ui() {
        let (_dir, store) = store();
        assert!(!store.root().exists());
        let installed = store.status().expect("status");
        assert_eq!(installed, Installed::BuiltIn);
        assert_eq!(installed.describe(), NO_CUSTOM_BUNDLE);
        assert!(!installed.describe().is_empty());
    }

    /// The root is created by the install path, never at start-up (§5.2).
    #[test]
    fn reading_status_never_creates_the_root() {
        let (_dir, store) = store();
        store.status().expect("status");
        store.discover_staged().expect("discover");
        store.recheck_active(&SERVED).expect("recheck");
        assert!(!store.root().exists());
    }

    /// §6.1 class 2, first half.
    #[test]
    fn rejects_a_tree_with_no_index() {
        let (_dir, store) = store();
        let staging = store.staging_dir(1);
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(staging.join("app.js"), b"x").expect("write asset");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(rejection(&err), Rejection::MissingIndex);
        assert_eq!(store.status().expect("status"), Installed::BuiltIn);
    }

    /// §6.1 class 2, second half: "or whose index is a directory".
    #[test]
    fn rejects_a_tree_whose_index_is_a_directory() {
        let (_dir, store) = store();
        let staging = store.staging_dir(1);
        fs::create_dir_all(staging.join(INDEX_NAME)).expect("create index dir");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(rejection(&err), Rejection::IndexNotRegularFile);
    }

    /// §5.3 rule 2 / §4.4 rule 5: a symlink is removed from the tree at
    /// unpack, before anything is reachable.
    #[test]
    fn rejects_a_staged_tree_containing_a_symlink() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        symlink("/etc/passwd", staging.join("assets/passwd")).expect("symlink");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(
            rejection(&err),
            Rejection::IrregularEntry {
                path: PathBuf::from("assets/passwd"),
                kind: EntryKind::Symlink,
            }
        );
        assert_eq!(store.status().expect("status"), Installed::BuiltIn);
    }

    /// §5.3 rule 2: no FIFOs.
    #[test]
    fn rejects_a_staged_tree_containing_a_fifo() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        let fifo = staging.join("assets/pipe");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed: {status}");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(
            rejection(&err),
            Rejection::IrregularEntry {
                path: PathBuf::from("assets/pipe"),
                kind: EntryKind::Fifo,
            }
        );
    }

    /// §5.3 rule 2: no sockets.
    #[test]
    fn rejects_a_staged_tree_containing_a_socket() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        let socket = staging.join("assets/sock");
        let listener = UnixListener::bind(&socket).expect("bind unix socket");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        drop(listener);
        assert_eq!(
            rejection(&err),
            Rejection::IrregularEntry {
                path: PathBuf::from("assets/sock"),
                kind: EntryKind::Socket,
            }
        );
    }

    /// §5.3 rule 2: no hardlinks. A hardlink is a regular file, so the only
    /// thing that distinguishes it is its link count.
    #[test]
    fn rejects_a_staged_tree_containing_a_hardlink() {
        let (dir, store) = store();
        let staging = stage_valid(&store, 1);
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"secret").expect("write outside file");
        fs::hard_link(&outside, staging.join("assets/link")).expect("hard link");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(
            rejection(&err),
            Rejection::Hardlink {
                path: PathBuf::from("assets/link"),
                links: 2,
            }
        );
    }

    /// §5.3: a bundle with no manifest is valid. Degradation, not rejection.
    #[test]
    fn a_tree_without_a_manifest_activates_unchecked() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        let activation = store.activate(1, &SERVED).expect("activate");
        assert_eq!(activation.compat, CompatCheck::NotRun);
        assert_eq!(activation.manifest, None);
        assert_eq!(
            fs::read_link(store.current_link()).expect("read current"),
            PathBuf::from("bundles/1")
        );

        let installed = store.status().expect("status");
        let ui = custom(&installed);
        assert_eq!(ui.generation, 1);
        assert_eq!(ui.manifest, None);
        assert!(ui.index_readable);
        let recorded = ui.recorded.as_ref().expect("an activation record");
        assert_eq!(recorded.compat, CompatCheck::NotRun);
        assert!(recorded.digest_matches);
        assert_eq!(installed.describe(), "generation 1 is active: no manifest");
    }

    /// §5.2: 0755 on directories, 0644 on files, on the installed tree.
    #[test]
    fn the_installed_tree_carries_the_layout_modes() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        fs::set_permissions(
            staging.join("assets/app.js"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("chmod asset");
        store.activate(1, &SERVED).expect("activate");
        let bundle = store.bundle_dir(1);
        let mode = |path: PathBuf| {
            fs::symlink_metadata(path)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(bundle.clone()), DIR_MODE);
        assert_eq!(mode(bundle.join("assets")), DIR_MODE);
        assert_eq!(mode(bundle.join("assets/app.js")), FILE_MODE);
        assert_eq!(mode(bundle.join(INDEX_NAME)), FILE_MODE);
    }

    /// §6.1 class 5: a declared range that intersects the served set activates.
    #[test]
    fn a_manifest_intersecting_the_served_set_activates() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        write_manifest(&staging, &["v2"]);
        let activation = store.activate(1, &SERVED).expect("activate");
        assert_eq!(
            activation.compat,
            CompatCheck::Ran {
                declared: vec!["v2".to_string()],
                served: vec!["v1".to_string(), "v2".to_string()],
                compatible: true,
            }
        );
        let installed = store.status().expect("status");
        let ui = custom(&installed);
        assert_eq!(
            ui.manifest,
            Some(ManifestSummary {
                name: "demo".to_string(),
                version: "1.2.3".to_string(),
            })
        );
    }

    /// §6.1 class 5: an empty intersection, and nothing else, refuses.
    #[test]
    fn a_manifest_with_an_empty_intersection_is_refused() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        write_manifest(&staging, &["v0"]);
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(
            rejection(&err),
            Rejection::Incompatible {
                declared: vec!["v0".to_string()],
                served: vec!["v1".to_string(), "v2".to_string()],
            }
        );
        assert_eq!(store.status().expect("status"), Installed::BuiltIn);
        assert!(!store.bundle_dir(1).exists());
    }

    /// The single most important negative test here: the relation is
    /// **membership in the served set**, not equality with `current`. A bundle
    /// built against the outgoing major must survive exactly the generation
    /// §2.1's dual-major recommendation exists to protect.
    #[test]
    fn a_manifest_matching_only_the_non_current_member_activates() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        // Served set ["v1", "v2"], `current` is "v2"; this bundle knows only v1.
        write_manifest(&staging, &["v1"]);
        let activation = store.activate(1, &SERVED).expect("must activate");
        assert_eq!(
            activation.compat,
            CompatCheck::Ran {
                declared: vec!["v1".to_string()],
                served: vec!["v1".to_string(), "v2".to_string()],
                compatible: true,
            }
        );
        let recheck = store
            .recheck_active(&SERVED)
            .expect("recheck")
            .expect("a bundle is active");
        assert!(!recheck.incompatible());
    }

    /// The same bundle after an A/B update to an image serving only `v2`:
    /// start-up's re-check now finds an empty intersection (§6.1 class 5).
    #[test]
    fn the_startup_recheck_finds_the_empty_intersection_after_an_update() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        write_manifest(&staging, &["v1"]);
        store.activate(1, &SERVED).expect("activate");

        let recheck = store
            .recheck_active(&["v2"])
            .expect("recheck")
            .expect("a bundle is active");
        assert!(recheck.incompatible());
        assert_eq!(
            recheck.compat,
            CompatCheck::Ran {
                declared: vec!["v1".to_string()],
                served: vec!["v2".to_string()],
                compatible: false,
            }
        );
        // The re-check reports; it never deactivates on its own.
        assert!(store.current_link().exists());
    }

    /// A bundle that could not be checked is never reported incompatible.
    #[test]
    fn the_startup_recheck_never_deactivates_an_unchecked_bundle() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        let recheck = store
            .recheck_active(&["v9"])
            .expect("recheck")
            .expect("a bundle is active");
        assert_eq!(recheck.compat, CompatCheck::NotRun);
        assert!(!recheck.incompatible());
        assert!(!recheck.corrupt());
    }

    /// §5.3 step 2: a manifest that is present must parse.
    #[test]
    fn a_present_but_unparsable_manifest_is_refused() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        fs::write(staging.join(MANIFEST_NAME), b"{not json").expect("write manifest");
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert!(matches!(rejection(&err), Rejection::ManifestUnparsable(_)));
    }

    /// §5.3 / §6.3: deactivate removes the pointer and keeps the tree.
    #[test]
    fn deactivate_keeps_the_bundle_on_disk() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");

        assert!(store.deactivate().expect("deactivate"));
        assert_eq!(store.status().expect("status"), Installed::BuiltIn);
        assert_eq!(store.status().expect("status").describe(), NO_CUSTOM_BUNDLE);
        assert!(store.bundle_dir(1).join(INDEX_NAME).exists());
        assert!(!store.current_link().exists());
        // Idempotent: deactivating twice is not an error.
        assert!(!store.deactivate().expect("deactivate again"));
    }

    #[test]
    fn a_deactivated_bundle_is_reported_and_selected_only_after_revalidation() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        write_manifest(&staging, &["v1"]);
        store.activate(1, &SERVED).expect("activate");
        store.deactivate().expect("deactivate");

        let candidate = store
            .available_custom(&SERVED)
            .expect("inspect retained bundles")
            .expect("a retained bundle");
        assert_eq!(candidate.generation, 1);
        assert!(candidate.usable);
        assert_eq!(candidate.unavailable_reason, None);
        assert_eq!(candidate.digest_matches, Some(true));
        assert_eq!(candidate.compatible, Some(true));

        let selected = store
            .select_available_custom(&SERVED)
            .expect("select retained bundle")
            .expect("a usable retained bundle");
        assert!(selected.changed);
        assert_eq!(selected.candidate, candidate);
        assert_eq!(custom(&store.status().expect("status")).generation, 1);

        let selected_again = store
            .select_available_custom(&SERVED)
            .expect("select retained bundle again")
            .expect("the active bundle remains usable");
        assert!(!selected_again.changed);
    }

    #[test]
    fn a_corrupt_retained_bundle_is_visible_but_cannot_be_selected() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        store.deactivate().expect("deactivate");
        fs::write(store.bundle_dir(1).join("assets/app.js"), b"changed")
            .expect("corrupt retained bundle");

        let candidate = store
            .available_custom(&SERVED)
            .expect("inspect retained bundles")
            .expect("the unusable bundle remains visible");
        assert!(!candidate.usable);
        assert_eq!(
            candidate.unavailable_reason,
            Some(CandidateUnavailable::DigestMismatch)
        );
        assert_eq!(candidate.digest_matches, Some(false));
        assert!(
            store
                .select_available_custom(&SERVED)
                .expect("selection is a state, not an error")
                .is_none()
        );
        assert_eq!(store.active_generation().expect("read current"), None);
    }

    #[test]
    fn a_retained_bundle_is_rechecked_against_the_current_served_api_set() {
        let (_dir, store) = store();
        let staging = stage_valid(&store, 1);
        write_manifest(&staging, &["v1"]);
        store.activate(1, &SERVED).expect("activate");
        store.deactivate().expect("deactivate");

        let candidate = store
            .available_custom(&["v9"])
            .expect("inspect against the new served set")
            .expect("the incompatible bundle remains visible");
        assert!(!candidate.usable);
        assert_eq!(candidate.digest_matches, Some(true));
        assert_eq!(candidate.compatible, Some(false));
        assert_eq!(
            candidate.unavailable_reason,
            Some(CandidateUnavailable::Incompatible)
        );
        assert!(
            store
                .select_available_custom(&["v9"])
                .expect("selection is a state, not an error")
                .is_none()
        );
    }

    #[test]
    fn selection_skips_a_newer_unusable_generation() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate generation 1");
        stage_valid(&store, 2);
        store.activate(2, &SERVED).expect("activate generation 2");
        store.deactivate().expect("deactivate");
        fs::write(store.bundle_dir(2).join(INDEX_NAME), b"changed")
            .expect("corrupt newest generation");

        let candidate = store
            .available_custom(&SERVED)
            .expect("inspect retained bundles")
            .expect("generation 1 is still usable");
        assert_eq!(candidate.generation, 1);
        assert!(candidate.usable);
        let selected = store
            .select_available_custom(&SERVED)
            .expect("select retained bundle")
            .expect("generation 1 is selectable");
        assert_eq!(selected.candidate.generation, 1);
    }

    #[test]
    fn selection_repairs_a_pointer_that_only_looks_like_the_generation() {
        let (dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        store.deactivate().expect("deactivate");
        let unmanaged = dir.path().join("unmanaged/1");
        fs::create_dir_all(&unmanaged).expect("create unmanaged tree");
        symlink(&unmanaged, store.current_link()).expect("plant unmanaged current pointer");

        let selected = store
            .select_available_custom(&SERVED)
            .expect("select retained bundle")
            .expect("the managed bundle is usable");
        assert!(selected.changed);
        assert_eq!(
            fs::read_link(store.current_link()).expect("read repaired pointer"),
            PathBuf::from("bundles/1")
        );
    }

    /// §5.3: deactivate is reversible — that is what keeping two generations
    /// buys.
    #[test]
    fn a_deactivated_bundle_can_be_reactivated_from_disk() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        store.deactivate().expect("deactivate");
        store.point_current_at(1).expect("re-point current");
        assert_eq!(custom(&store.status().expect("status")).generation, 1);
    }

    /// §5.3: delete after deactivate removes the tree.
    #[test]
    fn delete_after_deactivate_removes_the_tree() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        store.deactivate().expect("deactivate");
        store.delete(1).expect("delete");
        assert!(!store.bundle_dir(1).exists());
        assert!(!store.trash_dir(1).exists());
        assert_eq!(store.generations().expect("generations"), Vec::<u64>::new());
    }

    /// §5.3: never unlink the tree `current` points at.
    #[test]
    fn delete_while_active_is_refused() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        let err = store.delete(1).expect_err("must be refused");
        assert_eq!(rejection(&err), Rejection::DeleteWhileActive(1));
        assert!(store.bundle_dir(1).join(INDEX_NAME).exists());
        assert_eq!(custom(&store.status().expect("status")).generation, 1);
    }

    /// Installed versions remain until an operator explicitly deletes them.
    #[test]
    fn activation_never_prunes_retained_generations() {
        let (_dir, store) = store();
        for generation in 1..=4 {
            stage_valid(&store, generation);
            store.activate(generation, &SERVED).expect("activate");
        }
        assert_eq!(store.generations().expect("generations"), vec![1, 2, 3, 4]);
        assert_eq!(custom(&store.status().expect("status")).generation, 4);
        assert!(store.bundle_dir(3).join(INDEX_NAME).exists());
        assert!(store.record_path(2).exists());
        assert!(store.record_path(3).exists());
    }

    #[test]
    fn upload_refuses_a_reused_name_and_version_with_different_content() {
        let (_dir, store) = store();
        let first = stage_valid(&store, 1);
        write_manifest(&first, &["v1"]);
        store.install(1, &SERVED, 100, 200).expect("install first");

        let second = stage_valid(&store, 2);
        write_manifest(&second, &["v1"]);
        fs::write(second.join("assets/app.js"), b"different").expect("change second package");
        let err = store
            .install(2, &SERVED, 100, 200)
            .expect_err("same identity with different content must fail");
        assert_eq!(
            rejection(&err),
            Rejection::NameVersionConflict {
                name: "demo".to_string(),
                version: "1.2.3".to_string(),
                generation: 1,
            }
        );
        assert!(!store.bundle_dir(2).exists());
    }

    #[test]
    fn upload_refuses_a_thirty_third_retained_generation() {
        let (_dir, store) = store();
        fs::create_dir_all(store.bundles_dir()).expect("create retained root");
        for generation in 1..=32 {
            fs::create_dir(store.bundle_dir(generation)).expect("create retained generation");
        }
        let staging = stage_valid(&store, 33);
        write_manifest(&staging, &["v1"]);
        let err = store
            .install(33, &SERVED, 100, 200)
            .expect_err("retention limit must fail closed");
        assert_eq!(rejection(&err), Rejection::RetentionLimit(32));
        assert!(!store.bundle_dir(33).exists());
    }

    /// §6.1 class 3: a corruption written in over a root shell is detected by
    /// the digest, at activation and at start-up — not per request.
    #[test]
    fn the_digest_recheck_detects_a_file_mutated_after_activation() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        assert_eq!(
            store
                .recheck_active(&SERVED)
                .expect("recheck")
                .expect("active")
                .digest_matches,
            Some(true)
        );

        fs::write(store.bundle_dir(1).join("assets/app.js"), b"console.log(2)")
            .expect("mutate the installed tree");

        let recheck = store
            .recheck_active(&SERVED)
            .expect("recheck")
            .expect("active");
        assert_eq!(recheck.digest_matches, Some(false));
        assert!(recheck.corrupt());
        let installed = store.status().expect("status");
        assert!(
            !custom(&installed)
                .recorded
                .as_ref()
                .expect("record")
                .digest_matches
        );
    }

    /// A tree that no longer walks — a symlink planted under `bundles/` — is a
    /// digest mismatch rather than an error, because start-up must hold a
    /// state and never return one.
    #[test]
    fn a_symlink_planted_after_activation_reads_as_a_mismatch() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        symlink("/etc/passwd", store.bundle_dir(1).join("passwd")).expect("symlink");
        let recheck = store
            .recheck_active(&SERVED)
            .expect("recheck")
            .expect("active");
        assert_eq!(recheck.digest_matches, Some(false));
    }

    /// §5.3: validation runs on the staged tree and never on the live one, so
    /// a refusal leaves the previously active bundle active and untouched.
    #[test]
    fn a_refused_activation_leaves_the_active_bundle_untouched() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        let first = store.activate(1, &SERVED).expect("activate");

        // Generation 2 is staged with no index.html.
        let staging = store.staging_dir(2);
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(staging.join("app.js"), b"x").expect("write asset");
        let err = store.activate(2, &SERVED).expect_err("must be refused");
        assert_eq!(rejection(&err), Rejection::MissingIndex);

        assert_eq!(
            fs::read_link(store.current_link()).expect("read current"),
            PathBuf::from("bundles/1")
        );
        let installed = store.status().expect("status");
        let ui = custom(&installed);
        assert_eq!(ui.generation, 1);
        assert!(ui.index_readable);
        let recorded = ui.recorded.as_ref().expect("record");
        assert!(recorded.digest_matches);
        assert_eq!(recorded.digest, first.digest);
        // The rejected tree is still staged, and never reached bundles/.
        assert!(!store.bundle_dir(2).exists());
        assert!(staging.join("app.js").exists());
    }

    /// Activation never overwrites an installed generation.
    #[test]
    fn activating_an_existing_generation_is_refused() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        store.activate(1, &SERVED).expect("activate");
        stage_valid(&store, 1);
        let err = store.activate(1, &SERVED).expect_err("must be refused");
        assert_eq!(rejection(&err), Rejection::GenerationExists(1));
    }

    /// §8.2 phase 4's second local install path: apid picks up a staged
    /// directory.
    #[test]
    fn pick_up_staged_activates_the_highest_staged_generation() {
        let (_dir, store) = store();
        assert!(
            store
                .pick_up_staged(&SERVED)
                .expect("nothing staged")
                .is_none()
        );

        stage_valid(&store, 2);
        stage_valid(&store, 7);
        assert_eq!(store.discover_staged().expect("discover"), vec![2, 7]);
        let activation = store
            .pick_up_staged(&SERVED)
            .expect("pick up")
            .expect("something staged");
        assert_eq!(activation.generation, 7);
        assert_eq!(custom(&store.status().expect("status")).generation, 7);
        assert_eq!(store.discover_staged().expect("discover"), vec![2]);
    }

    /// The digest is a property of the tree's contents and paths, not of the
    /// order the filesystem returned them in.
    #[test]
    fn the_digest_is_stable_across_two_identical_trees() {
        let (_dir, store) = store();
        stage_valid(&store, 1);
        let first = store.activate(1, &SERVED).expect("activate");
        stage_valid(&store, 2);
        let second = store.activate(2, &SERVED).expect("activate");
        assert_eq!(first.digest, second.digest);
    }
}
