//! Content typing and cache classification for asset responses
//! (`docs/design/api.md` §4.3).
//!
//! Two decisions, both taken from the filename and neither from the file's
//! contents:
//!
//! - [`content_type`] maps an extension through a **fixed allowlist compiled
//!   into apid**. Not `mime_guess`, which is what `tower_http`'s `ServeDir`
//!   uses: the appliance serves a handful of extensions, and the table being
//!   ours makes the unknown-extension behaviour a decision rather than a
//!   default. An unknown or absent extension is [`DEFAULT_CONTENT_TYPE`] —
//!   downloadable, so the failure is legible rather than silent.
//! - [`cache_class`] places a file in one of §4.3's classes.
//!
//! [`X_CONTENT_TYPE_OPTIONS`]/[`NOSNIFF`] go on **every** asset response, not
//! only the unknown ones. Without it a browser may sniff an uploaded file as
//! HTML and execute it on the management origin, which turns "upload a UI
//! bundle" into stored cross-site scripting against the origin that holds the
//! session cookie. The cookie is `HttpOnly`, so script cannot read it — and
//! same-origin script does not need to, because it can issue authenticated
//! requests directly.
//!
//! The table lives inside the verity-covered image (§6.2), so a customer
//! cannot extend it on device; growing it is an A/B update. That is why
//! `.wasm` and `.webmanifest` are here from the start — §4.3 names them as the
//! two that *break* rather than degrade on `application/octet-stream`
//! (`WebAssembly.instantiateStreaming` fails outright; an installable web app
//! will not install).

use std::path::Path;

/// Response header that turns off content sniffing.
pub const X_CONTENT_TYPE_OPTIONS: &str = "x-content-type-options";

/// The only value [`X_CONTENT_TYPE_OPTIONS`] is ever sent with.
pub const NOSNIFF: &str = "nosniff";

/// What an unknown or absent extension is served as.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// The allowlist. Extensions are matched case-insensitively.
const CONTENT_TYPES: &[(&str, &str)] = &[
    ("avif", "image/avif"),
    ("css", "text/css; charset=utf-8"),
    ("gif", "image/gif"),
    ("htm", "text/html; charset=utf-8"),
    ("html", "text/html; charset=utf-8"),
    ("ico", "image/x-icon"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("js", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("txt", "text/plain; charset=utf-8"),
    ("wasm", "application/wasm"),
    ("webmanifest", "application/manifest+json"),
    ("webp", "image/webp"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("xml", "application/xml"),
];

/// The extensions [`cache_class`] treats as an HTML document.
const HTML_EXTENSIONS: &[&str] = &["htm", "html"];

/// Content type for `path`, from its extension alone.
#[must_use]
pub fn content_type(path: &Path) -> &'static str {
    let Some(extension) = extension_of(path) else {
        return DEFAULT_CONTENT_TYPE;
    };
    CONTENT_TYPES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(extension))
        .map_or(DEFAULT_CONTENT_TYPE, |(_, content_type)| *content_type)
}

/// §4.3's cache classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheClass {
    /// Every HTML document, including `index.html` and the SPA fallback, and
    /// every `/api/` response.
    NoStore,
    /// Everything else: revalidate before reuse.
    NoCache,
    /// Files under the directory the bundle manifest declares immutable.
    Immutable,
}

impl CacheClass {
    /// The `Cache-Control` value for this class.
    #[must_use]
    pub const fn header_value(self) -> &'static str {
        match self {
            Self::NoStore => "no-store",
            Self::NoCache => "no-cache",
            Self::Immutable => "public, max-age=31536000, immutable",
        }
    }
}

/// Classify a bundle-relative path.
///
/// `immutable_dir` is the directory §5.3's optional `mos-ui.json` declares
/// cacheable; `None` is the default for a bundle that declares nothing, and it
/// means `no-cache` for everything. The manifest is read by the installer, not
/// here.
///
/// HTML wins over the immutable declaration unconditionally. That interaction
/// is the one that would silently break §4.3's operator criterion — an
/// operator who uploads a bundle and reloads must see the new UI without
/// clearing the cache, without a hard refresh and without an incognito window
/// — because a year-long `immutable` on the one document that names the hashed
/// asset filenames is unrecoverable from the server side.
#[must_use]
pub fn cache_class(relative_path: &Path, immutable_dir: Option<&Path>) -> CacheClass {
    if is_html(relative_path) {
        return CacheClass::NoStore;
    }
    match immutable_dir {
        // A declaration with no components would make the whole bundle
        // immutable by accident; §4.3 requires immutability to be opt-in for a
        // named directory.
        Some(dir) if dir.components().next().is_some() && relative_path.starts_with(dir) => {
            CacheClass::Immutable
        }
        _ => CacheClass::NoCache,
    }
}

/// Whether `path` names an HTML document.
#[must_use]
pub fn is_html(path: &Path) -> bool {
    extension_of(path).is_some_and(|extension| {
        HTML_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(extension))
    })
}

fn extension_of(path: &Path) -> Option<&str> {
    path.extension()?.to_str()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        CacheClass, DEFAULT_CONTENT_TYPE, NOSNIFF, X_CONTENT_TYPE_OPTIONS, cache_class,
        content_type, is_html,
    };

    #[test]
    fn the_allowlist_answers_the_extensions_a_bundle_ships() {
        for (name, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("assets/app.a1b2c3.js", "text/javascript; charset=utf-8"),
            ("assets/app.a1b2c3.mjs", "text/javascript; charset=utf-8"),
            ("assets/app.a1b2c3.css", "text/css; charset=utf-8"),
            ("assets/app.a1b2c3.js.map", "application/json"),
            ("data.json", "application/json"),
            ("logo.svg", "image/svg+xml"),
            ("logo.png", "image/png"),
            ("photo.jpg", "image/jpeg"),
            ("photo.jpeg", "image/jpeg"),
            ("anim.gif", "image/gif"),
            ("hero.webp", "image/webp"),
            ("hero.avif", "image/avif"),
            ("favicon.ico", "image/x-icon"),
            ("font.woff", "font/woff"),
            ("font.woff2", "font/woff2"),
            ("readme.txt", "text/plain; charset=utf-8"),
            ("feed.xml", "application/xml"),
        ] {
            assert_eq!(content_type(Path::new(name)), expected, "{name}");
        }
    }

    /// §4.3 names these two specifically: they break rather than degrade on
    /// `application/octet-stream`, and the table is inside verity, so a
    /// customer cannot add them on device.
    #[test]
    fn wasm_and_webmanifest_are_in_the_table_from_the_start() {
        assert_eq!(content_type(Path::new("app.wasm")), "application/wasm");
        assert_eq!(
            content_type(Path::new("site.webmanifest")),
            "application/manifest+json"
        );
    }

    #[test]
    fn an_unknown_or_absent_extension_is_octet_stream() {
        for name in [
            "bundle.xyz",
            "LICENSE",
            "assets/.gitignore",
            "archive.tar.zst",
            "app.",
        ] {
            assert_eq!(
                content_type(Path::new(name)),
                DEFAULT_CONTENT_TYPE,
                "{name}"
            );
        }
    }

    #[test]
    fn extensions_match_case_insensitively() {
        assert_eq!(content_type(Path::new("LOGO.PNG")), "image/png");
        assert_eq!(
            content_type(Path::new("Index.HtMl")),
            "text/html; charset=utf-8"
        );
        assert!(is_html(Path::new("Index.HTM")));
    }

    #[test]
    fn nosniff_is_one_header_with_one_value() {
        assert_eq!(X_CONTENT_TYPE_OPTIONS, "x-content-type-options");
        assert_eq!(NOSNIFF, "nosniff");
    }

    #[test]
    fn the_classes_carry_the_headers_the_table_names() {
        assert_eq!(CacheClass::NoStore.header_value(), "no-store");
        assert_eq!(CacheClass::NoCache.header_value(), "no-cache");
        assert_eq!(
            CacheClass::Immutable.header_value(),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn html_is_no_store_and_everything_undeclared_is_no_cache() {
        assert_eq!(
            cache_class(Path::new("index.html"), None),
            CacheClass::NoStore
        );
        assert_eq!(
            cache_class(Path::new("about.htm"), None),
            CacheClass::NoStore
        );
        assert_eq!(
            cache_class(Path::new("assets/app.a1b2c3.js"), None),
            CacheClass::NoCache
        );
    }

    #[test]
    fn the_immutable_class_is_opt_in_for_a_named_directory() {
        let declared = Some(Path::new("assets"));
        assert_eq!(
            cache_class(Path::new("assets/app.a1b2c3.js"), declared),
            CacheClass::Immutable
        );
        assert_eq!(
            cache_class(Path::new("assets/fonts/x.woff2"), declared),
            CacheClass::Immutable
        );
        // Undeclared bundle: the same file is no-cache.
        assert_eq!(
            cache_class(Path::new("assets/app.a1b2c3.js"), None),
            CacheClass::NoCache
        );
        // A sibling whose name merely starts with the same characters is not
        // under the declared directory.
        assert_eq!(
            cache_class(Path::new("assetsx/app.a1b2c3.js"), declared),
            CacheClass::NoCache
        );
        // A declaration with no components does not make the bundle immutable.
        assert_eq!(
            cache_class(Path::new("assets/app.a1b2c3.js"), Some(Path::new(""))),
            CacheClass::NoCache
        );
    }

    /// §4.3's operator criterion, at this layer. An `index.html` that landed
    /// inside the declared immutable directory must still be `no-store`: this
    /// is the one interaction that would let a reload show the old UI, and it
    /// is the whole subsection's acceptance.
    #[test]
    fn html_inside_the_immutable_directory_is_still_no_store() {
        let declared = Some(Path::new("assets"));
        assert_eq!(
            cache_class(Path::new("assets/index.html"), declared),
            CacheClass::NoStore
        );
        assert_eq!(
            cache_class(Path::new("index.html"), Some(Path::new(""))),
            CacheClass::NoStore
        );
        // And the fallback body is the same document, so it is classified the
        // same way (§4.2).
        assert_eq!(
            cache_class(Path::new("index.html"), declared),
            CacheClass::NoStore
        );
    }
}
