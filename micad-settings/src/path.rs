//! Dot-path navigation over `serde_json` trees.
//!
//! A path is a `.`-separated list of segments. A segment is either *bare* —
//! no `.` and no `"` — or *double-quoted*, in which `.` is an ordinary
//! character: `network."eth0.100".dhcp` addresses the key `eth0.100`. The
//! spelling is TOML's own quoted-key syntax, which is what the store already
//! writes for such a key (`[network."eth0.100"]`), so an operator reading
//! `settings.toml` and an operator typing a path see one notation.
//!
//! Reads and writes share [`split_path`]: a path that resolves for `get`
//! is spelled exactly the way it is spelled for `set`.

use serde_json::{Map, Value};

use crate::error::SettingsError;

/// Resolve a dot-path (e.g. `"network.eth0.dhcp"`) inside a JSON tree.
///
/// `""` or `"."` return `root` itself. Returns `None` when a segment is
/// missing, empty, malformed, or traverses a non-object node.
pub fn json_path_get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() || path == "." {
        return Some(root);
    }
    let segments = split_path(path).ok()?;
    let mut node = root;
    for segment in segments {
        node = node.as_object()?.get(&segment)?;
    }
    Some(node)
}

/// Spell `segment` the way a path must carry it: bare when it can be, quoted
/// when it contains a `.`.
///
/// A segment containing `"` has no spelling; it is returned quoted anyway,
/// which [`split_path`] then rejects, so a path built from it fails
/// rather than addressing some other key. No validated writer can produce
/// such a key: apid, the network reconciler and [`Settings::set`] all refuse
/// it.
///
/// [`Settings::set`]: crate::Settings::set
#[must_use]
pub fn quote_path_segment(segment: &str) -> String {
    if segment.contains('.') || segment.contains('"') {
        format!("\"{segment}\"")
    } else {
        segment.to_string()
    }
}

/// The segments of `path`, unquoted, or `None` when `path` is not a
/// well-formed dot-path.
///
/// The public half of [`split_path`], for a caller that has to tell a
/// malformed path from a well-formed one *before* deciding an answer. apid's
/// settings write route is that caller: the record /// **404** to a path that names nothing and **422** to a path that is not a
/// path, and those two conditions are indistinguishable from below because
/// [`split_path`] and [`Settings::get`] both report a malformed path as
/// [`SettingsError::NotFound`].
///
/// [`Settings::get`]: crate::Settings::get
#[must_use]
pub fn path_segments(path: &str) -> Option<Vec<String>> {
    split_path(path).ok()
}

/// Split a non-root path into its segments, unquoting quoted ones.
///
/// # Errors
///
/// Returns [`SettingsError::NotFound`] for an empty segment, an unterminated
/// quoted segment, a `"` inside a bare segment, or trailing text after a
/// closing quote.
pub(crate) fn split_path(path: &str) -> Result<Vec<String>, SettingsError> {
    let malformed = || SettingsError::NotFound(path.to_string());
    let mut segments = Vec::new();
    let mut rest = path;
    loop {
        let (segment, tail) = if let Some(quoted) = rest.strip_prefix('"') {
            let (segment, tail) = quoted.split_once('"').ok_or_else(malformed)?;
            let tail = match tail.strip_prefix('.') {
                Some(tail) => Some(tail),
                None if tail.is_empty() => None,
                // Text between the closing quote and the next separator.
                None => return Err(malformed()),
            };
            (segment, tail)
        } else {
            match rest.split_once('.') {
                Some((segment, tail)) => (segment, Some(tail)),
                None => (rest, None),
            }
        };
        if segment.is_empty() || segment.contains('"') {
            return Err(malformed());
        }
        segments.push(segment.to_string());
        match tail {
            Some(tail) => rest = tail,
            None => return Ok(segments),
        }
    }
}

/// Write `value` at `segments`, creating missing intermediate objects.
pub(crate) fn json_path_set(
    root: &mut Value,
    segments: &[String],
    value: Value,
) -> Result<(), SettingsError> {
    let (last, parents) = segments
        .split_last()
        .expect("split_path yields at least one segment");
    let mut node = root;
    let mut walked = String::new();
    for segment in parents {
        let object = node.as_object_mut().ok_or_else(|| not_a_table(&walked))?;
        node = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !walked.is_empty() {
            walked.push('.');
        }
        walked.push_str(&quote_path_segment(segment));
    }
    let object = node.as_object_mut().ok_or_else(|| not_a_table(&walked))?;
    object.insert(last.clone(), value);
    Ok(())
}

fn not_a_table(path: &str) -> SettingsError {
    SettingsError::Validation {
        path: path.to_string(),
        message: "not a table".to_string(),
    }
}
