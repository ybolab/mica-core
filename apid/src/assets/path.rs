//! Request-path resolution for a static asset bundle (`docs/design/api.md`
//! §4.4).
//!
//! [`resolve`] is a pure function from a request target and an
//! **already-resolved** bundle root to a path inside that root. It performs
//! §4.4's five rules in order:
//!
//! 1. Reject before touching the filesystem — percent-decode, parse the
//!    decoded form into components, and require every component to be an
//!    ordinary name. The path is never built by string concatenation.
//! 2. Decode exactly once, and validate the decoded form. The decoded bytes
//!    are never re-scanned for escapes; a `%` that survives one decode is a
//!    rejection ([`Rejection::ResidualEscape`]) rather than an invitation to
//!    decode again.
//! 3. Strip *all* leading separators, then require every remaining component
//!    to be an ordinary name, so a root component anywhere is a rejection.
//! 4. Reject NUL explicitly, at the same place as `..`, rather than leaving
//!    the answer to a platform's `EINVAL`.
//! 5. Canonicalise the opened path and assert it is the path that was asked
//!    for, inside the root. This is §4.4's defence in depth; the primary
//!    defence is the unpacker refusing non-regular entries at install time
//!    (§5.3 requirement 2).
//!
//! **The root must already be resolved.** §4.4 carries one consequence into
//! §5.3: the assertion is made against the resolved bundle root, never against
//! the `/srv/ui/current` symlink, which is appliance-managed and lives outside
//! every bundle tree. Passing a non-canonical root is therefore a caller bug,
//! and it fails closed: every request under it is rejected, as
//! [`Rejection::OutsideRoot`] or [`Rejection::Symlink`] depending on the
//! shape.

use std::path::{Component, Path, PathBuf};

/// Why a request path did not resolve to a path inside the bundle.
///
/// Every variant except [`Rejection::NotFound`] names a guard that fired: the
/// request was hostile or malformed, and no fallback applies to it. See
/// [`Rejection::eligible_for_fallback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// A `%` that is not followed by two hex digits (`%`, `%2`, `%zz`).
    MalformedEscape,
    /// The decoded bytes are not valid UTF-8.
    NotUtf8,
    /// The decoded path contains a NUL byte (rule 4).
    Nul,
    /// A `%` survived the single decode pass — the request was encoded twice
    /// (`%252e`, `%2500`). Decoding it again is exactly what rule 2 forbids.
    ResidualEscape,
    /// The decoded path contains a backslash. Rule 4's reasoning applied to a
    /// second byte: on Linux `\` is an ordinary filename character and on
    /// Windows it is a separator, and a rule beats a platform-dependent
    /// answer. No asset in a bundle needs one.
    Backslash,
    /// A `..` component, at any position (rules 1 and 3).
    ParentDir,
    /// A root or prefix component, at any position (rules 1 and 3).
    RootOrPrefix,
    /// The path resolved through a symlink that stays inside the bundle. The
    /// rule is "no symlinks in bundle contents" (§5.3 requirement 2), not "no
    /// escaping symlinks".
    Symlink,
    /// The path resolved to something outside the bundle root (rule 5).
    OutsideRoot,
    /// Nothing exists at the requested path. The request itself was
    /// well-formed.
    NotFound,
}

impl Rejection {
    /// Whether the asset router may answer this with §4.2's SPA fallback.
    ///
    /// Only [`Rejection::NotFound`] may: a request the guards rejected must
    /// never come back as `200 text/html`. §4.2's conditions alone do not
    /// separate the two — `/%252e%252e%2fetc%2fpasswd` ends in a segment with
    /// no `.` and a browser navigation offers `text/html`, so it satisfies
    /// every one of them.
    #[must_use]
    pub const fn eligible_for_fallback(self) -> bool {
        matches!(self, Self::NotFound)
    }
}

/// Resolve a request target against an already-resolved bundle root.
///
/// The returned path is `bundle_root` plus the request's ordinary name
/// components, and it exists. It is not required to be a regular file: a
/// request for a directory resolves to that directory and the caller decides
/// what to serve for it.
///
/// # Errors
///
/// Returns the [`Rejection`] naming the single guard that fired.
pub fn resolve(request_path: &str, bundle_root: &Path) -> Result<PathBuf, Rejection> {
    // Rule 2: exactly one decode pass. `decoded` is never fed back in.
    let decoded = decode_once(request_path)?;

    // Rule 4, and the two byte-level rules that share its reasoning. Checked
    // on the decoded form, before the filesystem is touched.
    if decoded.as_bytes().contains(&0) {
        return Err(Rejection::Nul);
    }
    if decoded.contains('%') {
        return Err(Rejection::ResidualEscape);
    }
    if decoded.contains('\\') {
        return Err(Rejection::Backslash);
    }

    // Rule 3: strip *all* leading separators.
    let trimmed = decoded.trim_start_matches('/');

    // Rule 1: every remaining component must be an ordinary name.
    let mut relative = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(Rejection::ParentDir),
            // On Unix this arm is unreachable given the strip above, and no
            // test can distinguish its presence — the two halves of rule 3
            // are mutually redundant here. It is kept because §4.4 states it
            // as a rule and because it is what catches `/etc/passwd` if the
            // strip is ever weakened to a single separator.
            Component::RootDir | Component::Prefix(_) => return Err(Rejection::RootOrPrefix),
        }
    }

    // `join` of a relative path built from `Component::Normal` only; never
    // string concatenation, and never a push that an absolute path could
    // replace.
    let candidate = bundle_root.join(&relative);

    // Rule 5, defence in depth: what the kernel resolves must be what was
    // asked for.
    let canonical = candidate.canonicalize().map_err(|_| Rejection::NotFound)?;
    if !canonical.starts_with(bundle_root) {
        return Err(Rejection::OutsideRoot);
    }
    if canonical != candidate {
        return Err(Rejection::Symlink);
    }
    Ok(canonical)
}

/// Percent-decode `raw` exactly once.
///
/// The output is returned without being re-scanned for `%`, which is the half
/// of rule 2 that a general-purpose decoder cannot promise.
fn decode_once(raw: &str) -> Result<String, Rejection> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_nibble);
            let lo = bytes.get(i + 2).copied().and_then(hex_nibble);
            match (hi, lo) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => return Err(Rejection::MalformedEscape),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Rejection::NotUtf8)
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{Rejection, resolve};

    /// A bundle root with one decoy per escape a hostile request could aim at.
    ///
    /// `etc/passwd` exists *inside* the tree deliberately: a request that
    /// decodes to an absolute-looking `/etc/passwd` must resolve to this file
    /// and not to the real one, which is the difference between joining
    /// components and concatenating strings.
    struct Fixture {
        root: PathBuf,
        outside_file: PathBuf,
        _root_dir: TempDir,
        _outside_dir: TempDir,
    }

    fn fixture() -> Fixture {
        let root_dir = TempDir::new().expect("temp bundle root");
        let outside_dir = TempDir::new().expect("temp directory outside the bundle");
        let root = root_dir
            .path()
            .canonicalize()
            .expect("canonical bundle root");
        let outside = outside_dir
            .path()
            .canonicalize()
            .expect("canonical outside directory");

        let outside_file = outside.join("settings.toml");
        fs::write(&outside_file, "secret").expect("write outside file");

        fs::write(root.join("index.html"), "<!doctype html>").expect("write index.html");
        fs::create_dir(root.join("assets")).expect("create assets/");
        fs::write(root.join("assets/app.a1b2c3.js"), "//").expect("write hashed asset");
        fs::write(root.join("assets/index.html"), "<!doctype html>").expect("write nested index");
        fs::create_dir(root.join("etc")).expect("create etc/");
        fs::write(root.join("etc/passwd"), "decoy").expect("write decoy");
        fs::create_dir(root.join("a")).expect("create a/");
        fs::write(root.join("a/b"), "b").expect("write a/b");

        symlink(&outside_file, root.join("leak")).expect("plant escaping symlink");
        symlink("index.html", root.join("inside")).expect("plant non-escaping symlink");
        symlink("assets", root.join("linkdir")).expect("plant directory symlink");

        Fixture {
            root,
            outside_file,
            _root_dir: root_dir,
            _outside_dir: outside_dir,
        }
    }

    /// Every hostile input, the single guard it exercises, and what would
    /// happen if that one guard were deleted.
    ///
    /// The rows are deliberately *not* the compound strings §4.4 lists.
    /// `/%252e%252e%2fetc%2fpasswd` trips rules 1, 2 and 3 at once, so deleting
    /// any two of them leaves the test passing, which is the regression this
    /// table exists to catch. Each row below carries exactly one hostile
    /// feature, so the assertion on its variant is an assertion about one
    /// guard.
    #[test]
    fn each_guard_is_exercised_by_exactly_one_hostile_feature() {
        let f = fixture();

        // (input, expected, guard, what a missing guard would do)
        let cases: &[(&str, Rejection, &str)] = &[
            // Rule 1/3 — a `..` component, in each of the three positions.
            (
                "/../../etc/passwd",
                Rejection::ParentDir,
                "rule 1: `..` leading",
            ),
            (
                "/assets/../../etc/passwd",
                Rejection::ParentDir,
                "rule 3: `..` interior — `anywhere, not only at the front`",
            ),
            ("/assets/..", Rejection::ParentDir, "rule 3: `..` trailing"),
            // Rule 2, first half — decode happens BEFORE the component parse.
            // Validating the raw string for a literal `..` lets this through.
            (
                "/%2e%2e%2fetc%2fpasswd",
                Rejection::ParentDir,
                "rule 2: single-encoded `..`, decoded before parsing",
            ),
            (
                "/%2e%2e",
                Rejection::ParentDir,
                "rule 2: single-encoded `..`, alone",
            ),
            // Rule 2, second half — the decode is NOT repeated. A second pass
            // turns each of these back into the row above it.
            (
                "/%252e%252e%2fetc%2fpasswd",
                Rejection::ResidualEscape,
                "rule 2: double-encoded `..`, decoded exactly once",
            ),
            (
                "/%252e",
                Rejection::ResidualEscape,
                "rule 2: double-encoded, alone",
            ),
            (
                "/%2500",
                Rejection::ResidualEscape,
                "rule 2: double-encoded NUL",
            ),
            // Rule 4 — NUL rejected explicitly, not left to the syscall.
            ("/index.html%00.txt", Rejection::Nul, "rule 4: NUL as `%00`"),
            (
                "/index.html\u{0}.txt",
                Rejection::Nul,
                "rule 4: NUL as a raw byte",
            ),
            // Rule 4's reasoning applied to the backslash.
            ("/a\\b.js", Rejection::Backslash, "backslash, raw"),
            (
                "/%5cetc%5cpasswd",
                Rejection::Backslash,
                "backslash, encoded",
            ),
            // The decode itself.
            ("/%", Rejection::MalformedEscape, "truncated escape `%`"),
            ("/%2", Rejection::MalformedEscape, "truncated escape `%2`"),
            ("/%zz", Rejection::MalformedEscape, "non-hex escape `%zz`"),
            (
                "/%2gindex.html",
                Rejection::MalformedEscape,
                "half-hex escape `%2g`",
            ),
            ("/%c0%af", Rejection::NotUtf8, "overlong UTF-8 escape"),
            // Rule 5 — both directions, because the rule is "no symlinks in
            // bundle contents", not "no escaping symlinks".
            (
                "/leak",
                Rejection::OutsideRoot,
                "rule 5: symlink out of the tree",
            ),
            (
                "/inside",
                Rejection::Symlink,
                "rule 5: symlink within the tree",
            ),
            (
                "/linkdir/app.a1b2c3.js",
                Rejection::Symlink,
                "rule 5: symlinked intermediate directory",
            ),
        ];

        for (input, expected, guard) in cases {
            let got = resolve(input, &f.root);
            assert_eq!(
                got,
                Err(*expected),
                "{input:?} must be rejected by {guard}, got {got:?}"
            );
            assert!(
                !got.unwrap_err().eligible_for_fallback(),
                "{input:?} ({guard}) must never reach the SPA fallback"
            );
        }
    }

    /// Rule 2 stated as its own property: one decode, not two.
    ///
    /// This is the assertion the compound §4.4 string cannot make. If the
    /// decoder ran twice, `%252e%252e` would become `..` and the answer would
    /// be `ParentDir` — a rejection, so a test that only asserted "rejected"
    /// would still pass while rule 2 was gone.
    #[test]
    fn double_encoding_is_decoded_exactly_once() {
        let f = fixture();
        let got = resolve("/%252e%252e%2fetc%2fpasswd", &f.root);
        assert_eq!(got, Err(Rejection::ResidualEscape));
        assert_ne!(
            got,
            Err(Rejection::ParentDir),
            "a `..` verdict here means the path was decoded twice"
        );
        assert_ne!(
            resolve("/%2500", &f.root),
            Err(Rejection::Nul),
            "a NUL verdict here means `%2500` was decoded twice"
        );
    }

    /// Rule 3 stated as its own property: an absolute-looking decoded path is
    /// joined under the root, never concatenated and never allowed to replace
    /// it.
    ///
    /// `PathBuf::push` of an absolute path discards everything before it, so a
    /// single misplaced `push` turns this request into a read of the real
    /// `/etc/passwd`. The decoy inside the fixture is what makes the
    /// difference observable.
    #[test]
    fn a_decoded_leading_separator_stays_under_the_root() {
        let f = fixture();
        assert_eq!(
            resolve("/%2fetc%2fpasswd", &f.root),
            Ok(f.root.join("etc/passwd")),
            "must resolve to the decoy inside the bundle, not to /etc/passwd"
        );
        assert_eq!(
            resolve("///etc/passwd", &f.root),
            Ok(f.root.join("etc/passwd")),
            "all leading separators are stripped, not just the first"
        );
        // The same request with no decoy present is a plain miss, not an
        // escape: the real /etc/shadow is not reachable from here.
        assert_eq!(
            resolve("/%2fetc%2fshadow", &f.root),
            Err(Rejection::NotFound)
        );
    }

    /// An encoded separator is decoded before the component parse, so it
    /// becomes a separator — it is not smuggled through as part of a name.
    /// This is the same ordering rule 2 relies on, observed without any `..`.
    #[test]
    fn an_encoded_separator_becomes_a_separator() {
        let f = fixture();
        assert_eq!(resolve("/a%2fb", &f.root), Ok(f.root.join("a/b")));
        assert_eq!(resolve("/a/b", &f.root), Ok(f.root.join("a/b")));
    }

    /// The other direction. A suite that only proves refusals passes just as
    /// well against a function that refuses everything — the discipline
    /// `pkgs/mosd/tests/dbus-policy-test.sh` documents for the D-Bus policy.
    #[test]
    fn ordinary_paths_resolve() {
        let f = fixture();
        for (input, expected) in [
            ("/index.html", "index.html"),
            // A legitimate `.` in a filename is not a `..`, and the
            // content-hashed name is the shape §4.3's immutable class exists
            // for.
            ("/assets/app.a1b2c3.js", "assets/app.a1b2c3.js"),
            ("/./index.html", "index.html"),
            ("//index.html", "index.html"),
            ("/assets/", "assets"),
        ] {
            assert_eq!(
                resolve(input, &f.root),
                Ok(f.root.join(expected)),
                "{input:?} is an ordinary request and must resolve"
            );
        }
        assert_eq!(resolve("/", &f.root), Ok(f.root.clone()));
    }

    /// A miss is not a hostile request, and it is the one outcome §4.2's
    /// fallback may answer.
    #[test]
    fn a_plain_miss_is_the_only_fallback_eligible_outcome() {
        let f = fixture();
        let got = resolve("/settings/network", &f.root);
        assert_eq!(got, Err(Rejection::NotFound));
        assert!(got.unwrap_err().eligible_for_fallback());
    }

    /// The escaping symlink reaches a real file, so the rejection is the only
    /// thing standing between a bundle and an arbitrary read as root.
    #[test]
    fn the_escaping_symlink_target_is_readable_without_the_guard() {
        let f = fixture();
        assert_eq!(
            fs::read_to_string(&f.outside_file).expect("target readable"),
            "secret",
            "the fixture must actually point at a readable file, or the \
             symlink rejection proves nothing"
        );
        assert_eq!(resolve("/leak", &f.root), Err(Rejection::OutsideRoot));
    }

    /// A root that is itself a symlink fails closed rather than silently
    /// widening the assertion, which is what §4.4's "resolved bundle root"
    /// requires of the caller. The router resolves `/srv/ui/current` before
    /// calling in; this is what happens if it forgets.
    #[test]
    fn a_symlinked_root_rejects_everything() {
        let f = fixture();
        let unresolved_root = f.root.join("linkdir");
        assert_eq!(
            resolve("/app.a1b2c3.js", Path::new(&unresolved_root)),
            Err(Rejection::OutsideRoot)
        );
    }
}
