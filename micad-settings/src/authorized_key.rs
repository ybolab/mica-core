//! Parser and validator for SSH authorized keys.
//!
//! This module is a security boundary, not a convenience. Whatever it accepts
//! is written verbatim into a file sshd reads and acts on, so it parses the
//! narrowest grammar that still expresses a usable key — `<type> <blob>` with
//! an optional trailing comment — and rejects everything else, including
//! constructs OpenSSH itself would happily honour.

use std::collections::BTreeMap;

use crate::error::SettingsError;
use crate::model::AuthorizedKey;

/// Dot-path reported by every error raised here.
const KEYS_PATH: &str = "access.ssh.authorizedKeys";

/// Key types accepted, matched byte-for-byte.
///
/// The list is a whitelist rather than a "anything sshd knows" check: DSA and
/// the retired `ssh-rsa` SHA-1 signature variants are absent because a key the
/// operator cannot use is a smaller problem than a key that is weaker than the
/// operator believes.
const ACCEPTED_TYPES: [&str; 7] = [
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// Largest number of keys the settings tree will hold.
///
/// A bound has to exist because the list is rendered into a file read on every
/// login attempt; 32 is far past any real appliance and far short of a list an
/// operator could use to fill STATE.
///
/// **Public so a caller can answer the cap before this validator refuses it**,
/// which is what [`MAX_TOKENS`](crate::MAX_TOKENS) is public for. A route that
/// can read the bound answers a full collection as 409 — a conflict with the
/// collection's current state — instead of meeting it as a validation failure
/// with no way to tell it from a malformed key. the record /// this export as the alternative it declined; the error-contract
/// ruling picks it.
pub const MAX_KEYS: usize = 32;

/// Largest comment accepted, in bytes.
const MAX_COMMENT_BYTES: usize = 256;

/// Smallest decoded blob accepted, in bytes.
///
/// The shortest real public key blob (Ed25519) decodes to 51 bytes; 32 is a
/// floor that rejects truncated paste-ins without hard-coding per-type sizes.
const MIN_BLOB_BYTES: usize = 32;

/// Base64 alphabet used by both halves of the codec.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Parse one authorized-key line into its canonical form.
///
/// Accepts exactly `<type> <blob>` or `<type> <blob> <comment>`, one ASCII
/// space between fields. There is deliberately no support for the `options`
/// field OpenSSH allows in front of the type: `command=`, `environment=` and
/// friends are a remote-code-execution surface, so a line not beginning with a
/// known key type is rejected outright, including a `#` comment line. On
/// success `key` holds `"<type> <blob>"` with the comment stripped, so the same
/// key pasted under two labels compares equal.
///
/// # Errors
///
/// Returns [`SettingsError::Validation`] naming the offending field and the
/// byte offset within it. The rejected input is never echoed back into the
/// message.
pub fn parse_authorized_key(line: &str) -> Result<AuthorizedKey, SettingsError> {
    check_line_shape(line)?;

    let mut fields = line.splitn(3, ' ');
    let key_type = fields.next().unwrap_or_default();
    let blob = fields
        .next()
        .ok_or_else(|| invalid("line has no base64 blob: expected `<type> <blob> [comment]`"))?;
    let comment = fields.next();

    if !ACCEPTED_TYPES.contains(&key_type) {
        return Err(invalid(format!(
            "key type field is not one of the {} accepted types",
            ACCEPTED_TYPES.len()
        )));
    }
    check_blob(blob)?;
    let decoded = decode_base64(blob)
        .ok_or_else(|| invalid("base64 blob does not decode to a byte string"))?;
    if decoded.len() < MIN_BLOB_BYTES {
        return Err(invalid(format!(
            "base64 blob decodes to {} bytes, below the {MIN_BLOB_BYTES}-byte minimum",
            decoded.len()
        )));
    }
    check_blob_declares(&decoded, key_type)?;

    let comment = match comment {
        None => None,
        Some(comment) => {
            check_comment(comment)?;
            Some(comment.to_string())
        }
    };
    Ok(AuthorizedKey {
        key: format!("{key_type} {blob}"),
        comment,
    })
}

/// Validate a whole authorized-key list as it will be persisted.
///
/// Re-parses every entry rather than trusting the stored text: the settings
/// file is an editable file on STATE, so a key that only ever passed through
/// [`parse_authorized_key`] on the way in is not the same as a key that still
/// parses on the way out.
///
/// # Errors
///
/// Returns [`SettingsError::Validation`] naming the failing entry's index when
/// the list is too long, an entry does not re-parse, an entry carries its
/// comment inside `key`, a comment breaks the comment rules, or two entries
/// share a key.
pub fn validate_authorized_keys(keys: &[AuthorizedKey]) -> Result<(), SettingsError> {
    if keys.len() > MAX_KEYS {
        return Err(invalid(format!(
            "list holds {} keys, above the maximum of {MAX_KEYS}",
            keys.len()
        )));
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, entry) in keys.iter().enumerate() {
        let parsed = parse_authorized_key(&entry.key).map_err(|err| {
            let message = match err {
                SettingsError::Validation { message, .. } => message,
                other => other.to_string(),
            };
            invalid(format!("entry {index}: {message}"))
        })?;
        if parsed.comment.is_some() {
            return Err(invalid(format!(
                "entry {index}: `key` carries a comment; the comment belongs in the `comment` field"
            )));
        }
        if let Some(comment) = &entry.comment {
            check_comment(comment).map_err(|err| {
                let message = match err {
                    SettingsError::Validation { message, .. } => message,
                    other => other.to_string(),
                };
                invalid(format!("entry {index}: {message}"))
            })?;
        }
        if let Some(first) = seen.insert(entry.key.as_str(), index) {
            return Err(invalid(format!(
                "entry {index} duplicates the key already held by entry {first}"
            )));
        }
    }
    Ok(())
}

/// Decode standard-alphabet base64, with or without `=` padding.
///
/// Returns `None` for anything that is not a well-formed encoding — an
/// out-of-alphabet byte, more than two padding characters, padding that does
/// not land on a four-character boundary, or a body length that no byte string
/// can produce. Callers get a rejection, never a panic and never a partial
/// decode.
#[must_use]
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 || (padding > 0 && !bytes.len().is_multiple_of(4)) {
        return None;
    }
    let body = &bytes[..bytes.len() - padding];
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in body {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Encode bytes as standard-alphabet base64 without `=` padding.
#[must_use]
pub fn encode_base64_nopad(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut group: u32 = 0;
        for (index, byte) in chunk.iter().enumerate() {
            group |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..chunk.len() + 1 {
            let sextet = (group >> (18 - 6 * index)) & 0x3f;
            out.push(char::from(BASE64_ALPHABET[sextet as usize]));
        }
    }
    out
}

/// Reject bytes that must not appear anywhere in the line, and any leading or
/// trailing space.
///
/// A newline would turn one entry into two authorized keys, and a carriage
/// return or a NUL would let the visible prefix of an entry differ from what
/// sshd reads; the remaining control characters are banned because nothing in
/// a key needs them.
fn check_line_shape(line: &str) -> Result<(), SettingsError> {
    if line.is_empty() {
        return Err(invalid("line is empty"));
    }
    for (offset, ch) in line.char_indices() {
        let forbidden = match ch {
            '\0' => "NUL",
            '\n' => "line feed",
            '\r' => "carriage return",
            '\t' => "tab",
            '\u{b}' => "vertical tab",
            '\u{c}' => "form feed",
            _ => continue,
        };
        return Err(invalid(format!(
            "line holds a {forbidden} at byte {offset}"
        )));
    }
    if line.starts_with(' ') {
        return Err(invalid("line starts with a space"));
    }
    if line.ends_with(' ') {
        return Err(invalid("line ends with a space"));
    }
    Ok(())
}

/// Reject a blob that is not a syntactically valid base64 word.
///
/// An empty blob is how a double space between the type and the blob shows up
/// after splitting, so the message says so: the two are the same mistake.
fn check_blob(blob: &str) -> Result<(), SettingsError> {
    if blob.is_empty() {
        return Err(invalid(
            "base64 blob is empty; fields are separated by exactly one space",
        ));
    }
    let bytes = blob.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(invalid(format!(
            "base64 blob is {} characters, not a multiple of 4",
            bytes.len()
        )));
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return Err(invalid(format!(
            "base64 blob ends with {padding} padding characters, at most 2 are allowed"
        )));
    }
    for (offset, byte) in bytes[..bytes.len() - padding].iter().enumerate() {
        if !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/') {
            return Err(invalid(format!(
                "base64 blob holds a character outside `A-Za-z0-9+/` at byte {offset}"
            )));
        }
    }
    Ok(())
}

/// Reject a blob whose own algorithm name disagrees with the declared type.
///
/// An OpenSSH public key blob starts with a four-byte big-endian length
/// followed by that many bytes of algorithm name. Comparing it to the declared
/// type is what makes this module a validator rather than a shape check: a
/// line reading `ssh-ed25519 <an RSA blob>` is well-formed by every other rule
/// here, and sshd would resolve the mismatch by trusting the blob, not the
/// label the operator read.
fn check_blob_declares(decoded: &[u8], key_type: &str) -> Result<(), SettingsError> {
    let Some((length_bytes, rest)) = decoded.split_at_checked(4) else {
        return Err(invalid(
            "base64 blob is too short to hold an algorithm name",
        ));
    };
    let length = u32::from_be_bytes([
        length_bytes[0],
        length_bytes[1],
        length_bytes[2],
        length_bytes[3],
    ]);
    let name = usize::try_from(length)
        .ok()
        .and_then(|length| rest.get(..length))
        .ok_or_else(|| {
            invalid("base64 blob declares an algorithm name longer than the blob itself")
        })?;
    if name != key_type.as_bytes() {
        return Err(invalid(
            "algorithm name inside the base64 blob does not match the declared key type",
        ));
    }
    Ok(())
}

/// Reject a comment that could change how the rendered file parses, or that is
/// unbounded.
///
/// Everything from `0x20` up is allowed, including shell metacharacters and
/// non-ASCII UTF-8: the comment is never handed to a shell, and operators do
/// put names in it. What is banned is the C0 range and `0x7f`, which is the
/// set that could split a line or hide the rest of the entry from a reader.
fn check_comment(comment: &str) -> Result<(), SettingsError> {
    if comment.is_empty() {
        return Err(invalid(
            "comment is empty; omit the trailing space instead of leaving it blank",
        ));
    }
    if comment.len() > MAX_COMMENT_BYTES {
        return Err(invalid(format!(
            "comment is {} bytes, above the maximum of {MAX_COMMENT_BYTES}",
            comment.len()
        )));
    }
    for (offset, ch) in comment.char_indices() {
        if (ch as u32) < 0x20 || ch == '\u{7f}' {
            return Err(invalid(format!(
                "comment holds a control character at byte {offset}"
            )));
        }
    }
    Ok(())
}

/// Build the one error variant this module raises.
fn invalid(message: impl Into<String>) -> SettingsError {
    SettingsError::Validation {
        path: KEYS_PATH.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a structurally valid blob for `key_type`: the four-byte length,
    /// the algorithm name, then filler out to `total` decoded bytes.
    ///
    /// Padded to a four-character boundary because the parser demands a length
    /// that is a multiple of 4, which is what a real OpenSSH key always is.
    fn blob_for(key_type: &str, total: usize) -> String {
        let mut bytes = Vec::new();
        let name = key_type.as_bytes();
        bytes.extend_from_slice(&u32::try_from(name.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(name);
        // Filler is position-dependent so two blobs of the same length for
        // different types are not accidentally equal.
        while bytes.len() < total {
            let index = u8::try_from(bytes.len() % 251).unwrap();
            bytes.push(index.wrapping_mul(7).wrapping_add(3));
        }
        pad(&encode_base64_nopad(&bytes))
    }

    /// Append `=` until the encoding lands on a four-character boundary.
    fn pad(encoded: &str) -> String {
        let mut padded = encoded.to_string();
        while !padded.len().is_multiple_of(4) {
            padded.push('=');
        }
        padded
    }

    fn valid_line(key_type: &str) -> String {
        format!("{key_type} {}", blob_for(key_type, 64))
    }

    fn message_of(err: &SettingsError) -> String {
        match err {
            SettingsError::Validation { path, message } => {
                assert_eq!(path, KEYS_PATH);
                message.clone()
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    // --- Positive: every accepted type ------------------------------------

    #[test]
    fn every_accepted_type_parses_with_and_without_a_comment() {
        for key_type in ACCEPTED_TYPES {
            let blob = blob_for(key_type, 64);
            let bare = format!("{key_type} {blob}");

            let parsed = parse_authorized_key(&bare)
                .unwrap_or_else(|err| panic!("{key_type} should parse: {err}"));
            assert_eq!(parsed.key, bare, "{key_type} canonical form");
            assert_eq!(parsed.comment, None, "{key_type} has no comment");

            let labelled = format!("{key_type} {blob} alice@workstation");
            let parsed = parse_authorized_key(&labelled)
                .unwrap_or_else(|err| panic!("{key_type} with a comment should parse: {err}"));
            // The comment is stripped from `key`, not kept in it.
            assert_eq!(parsed.key, bare, "{key_type} strips the comment from `key`");
            assert_eq!(parsed.comment.as_deref(), Some("alice@workstation"));

            // Re-parsing the canonical form is a fixed point.
            assert_eq!(parse_authorized_key(&parsed.key).unwrap().key, bare);
        }
        assert_eq!(ACCEPTED_TYPES.len(), 7);
    }

    #[test]
    fn a_type_outside_the_whitelist_is_rejected() {
        // Each line carries a blob that agrees with its declared type, so the
        // whitelist is the only guard that can reject it; a mismatched blob
        // would let this test pass on the blob-vs-type check instead.
        for key_type in [
            "ssh-dss",
            "ssh-ed25519-cert-v01@openssh.com",
            "SSH-ED25519",
            "rsa-sha2-256",
            "webauthn-sk-ecdsa-sha2-nistp256@openssh.com",
        ] {
            let line = format!("{key_type} {}", blob_for(key_type, 96));
            let err = parse_authorized_key(&line).unwrap_err();
            assert!(
                message_of(&err).contains("key type"),
                "{key_type} must be rejected by the whitelist, got: {}",
                message_of(&err)
            );
        }
    }

    // --- Per-character hostile input --------------------------------------

    /// Characters that must be rejected wherever they appear, comment
    /// included.
    const CONTROL_CHARS: [(char, &str); 7] = [
        ('\0', "NUL"),
        ('\n', "line feed"),
        ('\r', "carriage return"),
        ('\t', "tab"),
        ('\u{b}', "vertical tab"),
        ('\u{c}', "form feed"),
        ('\u{7f}', "delete"),
    ];

    /// Characters a shell would treat as special, which this parser must not:
    /// they are rejected in the type and the blob because they are not in
    /// those alphabets, and accepted verbatim in a comment.
    const SHELL_METACHARS: [char; 6] = ['\\', '"', '\'', '`', '$', '#'];

    #[test]
    fn control_characters_are_rejected_in_the_type_field() {
        let blob = blob_for("ssh-ed25519", 64);
        for (ch, name) in CONTROL_CHARS {
            let line = format!("ssh-ed{ch}25519 {blob}");
            assert!(
                parse_authorized_key(&line).is_err(),
                "a {name} in the type field must be rejected"
            );
        }
    }

    #[test]
    fn control_characters_are_rejected_in_the_blob() {
        let blob = blob_for("ssh-ed25519", 64);
        for (ch, name) in CONTROL_CHARS {
            let mangled = format!("{}{ch}{}", &blob[..8], &blob[9..]);
            let line = format!("ssh-ed25519 {mangled}");
            assert!(
                parse_authorized_key(&line).is_err(),
                "a {name} in the blob must be rejected"
            );
        }
    }

    #[test]
    fn control_characters_are_rejected_in_the_comment() {
        let blob = blob_for("ssh-ed25519", 64);
        for (ch, name) in CONTROL_CHARS {
            let line = format!("ssh-ed25519 {blob} alice{ch}bob");
            assert!(
                parse_authorized_key(&line).is_err(),
                "a {name} in the comment must be rejected"
            );
        }
    }

    #[test]
    fn shell_metacharacters_are_rejected_in_the_type_and_the_blob() {
        let blob = blob_for("ssh-ed25519", 64);
        for ch in SHELL_METACHARS {
            let typed = format!("ssh-ed{ch}25519 {blob}");
            assert!(
                parse_authorized_key(&typed).is_err(),
                "`{ch}` in the type field must be rejected"
            );

            let mangled = format!("{}{ch}{}", &blob[..8], &blob[9..]);
            let blobbed = format!("ssh-ed25519 {mangled}");
            let err = parse_authorized_key(&blobbed).unwrap_err();
            assert!(
                message_of(&err).contains("base64 blob"),
                "`{ch}` in the blob must be rejected as a blob error, got {}",
                message_of(&err)
            );
        }
    }

    #[test]
    fn shell_metacharacters_are_accepted_verbatim_inside_a_comment() {
        // The comment reaches no shell. Rejecting these would be security
        // theatre that stops an operator writing `dev$box` as a label, so the
        // acceptance is asserted rather than left to chance.
        let blob = blob_for("ssh-ed25519", 64);
        for ch in SHELL_METACHARS {
            let comment = format!("alice{ch}bob");
            let line = format!("ssh-ed25519 {blob} {comment}");
            let parsed = parse_authorized_key(&line)
                .unwrap_or_else(|err| panic!("`{ch}` in a comment must be accepted: {err}"));
            assert_eq!(parsed.comment.as_deref(), Some(comment.as_str()));
            assert_eq!(parsed.key, format!("ssh-ed25519 {blob}"));
        }
    }

    #[test]
    fn a_comment_that_starts_with_a_hash_is_still_a_comment() {
        let blob = blob_for("ssh-ed25519", 64);
        let parsed = parse_authorized_key(&format!("ssh-ed25519 {blob} #1 laptop")).unwrap();
        assert_eq!(parsed.comment.as_deref(), Some("#1 laptop"));
    }

    #[test]
    fn non_ascii_utf8_in_a_comment_is_accepted() {
        let blob = blob_for("ssh-ed25519", 64);
        let parsed = parse_authorized_key(&format!("ssh-ed25519 {blob} Ada Lovelace (café)"))
            .expect("a non-ASCII label must be accepted");
        assert_eq!(parsed.comment.as_deref(), Some("Ada Lovelace (café)"));
    }

    #[test]
    fn space_runs_and_edge_spaces_are_rejected_between_fields_but_kept_in_a_comment() {
        let blob = blob_for("ssh-ed25519", 64);

        // Two spaces between the type and the blob.
        let err = parse_authorized_key(&format!("ssh-ed25519  {blob}")).unwrap_err();
        assert!(message_of(&err).contains("base64 blob is empty"));

        // A leading space, and a trailing one (which is also the empty-comment
        // case).
        let err = parse_authorized_key(&format!(" ssh-ed25519 {blob}")).unwrap_err();
        assert!(message_of(&err).contains("starts with a space"));
        let err = parse_authorized_key(&format!("ssh-ed25519 {blob} ")).unwrap_err();
        assert!(message_of(&err).contains("ends with a space"));

        // A run inside the comment is fine: everything after the second space
        // is the comment, verbatim.
        let parsed = parse_authorized_key(&format!("ssh-ed25519 {blob}  spaced  out")).unwrap();
        assert_eq!(parsed.comment.as_deref(), Some(" spaced  out"));
    }

    #[test]
    fn an_empty_comment_is_a_rejection_not_a_none() {
        let blob = blob_for("ssh-ed25519", 64);
        let err = parse_authorized_key(&format!("ssh-ed25519 {blob} ")).unwrap_err();
        assert!(message_of(&err).contains("space"));
        // The same line without the trailing space is the `None` case.
        assert_eq!(
            parse_authorized_key(&format!("ssh-ed25519 {blob}"))
                .unwrap()
                .comment,
            None
        );
    }

    #[test]
    fn a_comment_is_bounded_at_256_bytes() {
        let blob = blob_for("ssh-ed25519", 64);
        let at_limit = "c".repeat(256);
        assert_eq!(
            parse_authorized_key(&format!("ssh-ed25519 {blob} {at_limit}"))
                .unwrap()
                .comment
                .as_deref(),
            Some(at_limit.as_str())
        );

        let over = "c".repeat(257);
        let err = parse_authorized_key(&format!("ssh-ed25519 {blob} {over}")).unwrap_err();
        assert!(
            message_of(&err).contains("257 bytes"),
            "{}",
            message_of(&err)
        );
    }

    // --- Blob semantics ---------------------------------------------------

    #[test]
    fn the_blob_must_declare_the_type_the_line_declares() {
        // Matching: accepted.
        let matching = format!("ssh-ed25519 {}", blob_for("ssh-ed25519", 64));
        assert!(parse_authorized_key(&matching).is_ok());

        // Mismatched: an RSA blob presented as an Ed25519 key. Every other
        // rule here passes; only the embedded algorithm name disagrees.
        let mismatched = format!("ssh-ed25519 {}", blob_for("ssh-rsa", 64));
        let err = parse_authorized_key(&mismatched).unwrap_err();
        assert!(
            message_of(&err).contains("does not match the declared key type"),
            "{}",
            message_of(&err)
        );

        // And the other direction, so the test is not passing on a length
        // coincidence between the two names.
        let swapped = format!("ssh-rsa {}", blob_for("ssh-ed25519", 64));
        assert!(parse_authorized_key(&swapped).is_err());

        // Every accepted type mismatches every other one.
        for declared in ACCEPTED_TYPES {
            for embedded in ACCEPTED_TYPES {
                let line = format!("{declared} {}", blob_for(embedded, 96));
                assert_eq!(
                    parse_authorized_key(&line).is_ok(),
                    declared == embedded,
                    "declared {declared}, blob says {embedded}"
                );
            }
        }
    }

    #[test]
    fn a_blob_below_thirty_two_decoded_bytes_is_rejected_and_exactly_thirty_two_is_accepted() {
        // Literals rather than MIN_BLOB_BYTES: widening the constant must fail
        // a test, not silently move the expectation with it.
        let exactly = blob_for("ssh-ed25519", 32);
        assert_eq!(decode_base64(&exactly).unwrap().len(), 32);
        assert!(
            parse_authorized_key(&format!("ssh-ed25519 {exactly}")).is_ok(),
            "a blob of exactly 32 bytes must be accepted"
        );

        let short = blob_for("ssh-ed25519", 31);
        assert_eq!(decode_base64(&short).unwrap().len(), 31);
        let err = parse_authorized_key(&format!("ssh-ed25519 {short}")).unwrap_err();
        assert!(
            message_of(&err).contains("31 bytes"),
            "{}",
            message_of(&err)
        );

        // 16 bytes is a truncated paste rather than an off-by-one.
        let truncated = blob_for("ssh-ed25519", 16);
        assert!(parse_authorized_key(&format!("ssh-ed25519 {truncated}")).is_err());
    }

    #[test]
    fn a_blob_whose_length_is_not_a_multiple_of_four_is_rejected() {
        let blob = blob_for("ssh-ed25519", 64);
        let trimmed = &blob[..blob.len() - 1];
        let err = parse_authorized_key(&format!("ssh-ed25519 {trimmed}")).unwrap_err();
        assert!(
            message_of(&err).contains("not a multiple of 4"),
            "{}",
            message_of(&err)
        );
    }

    #[test]
    fn three_or_more_padding_characters_are_rejected() {
        let blob = blob_for("ssh-ed25519", 64);
        let overpadded = format!("{blob}====");
        let err = parse_authorized_key(&format!("ssh-ed25519 {overpadded}")).unwrap_err();
        assert!(message_of(&err).contains("padding"), "{}", message_of(&err));
    }

    // --- Lines that are not keys at all -----------------------------------

    #[test]
    fn options_hash_comments_and_empty_lines_are_rejected() {
        let blob = blob_for("ssh-ed25519", 64);
        let rejected = [
            format!("command=\"x\" ssh-ed25519 {blob}"),
            format!("no-pty ssh-ed25519 {blob}"),
            format!("restrict ssh-ed25519 {blob}"),
            format!("environment=\"A=b\" ssh-ed25519 {blob}"),
            format!("no-pty,command=\"x\" ssh-ed25519 {blob}"),
            format!("# ssh-ed25519 {blob}"),
            format!("#ssh-ed25519 {blob}"),
            String::new(),
            " ".to_string(),
            "ssh-ed25519".to_string(),
        ];
        for line in rejected {
            assert!(
                parse_authorized_key(&line).is_err(),
                "must be rejected: {} bytes starting `{}`",
                line.len(),
                line.chars().take(12).collect::<String>()
            );
        }
    }

    // --- base64 codec -----------------------------------------------------

    /// Deterministic pseudo-random bytes; a fixed LCG keeps the test
    /// reproducible without a dependency.
    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x1234_5678;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                u8::try_from((state >> 16) & 0xff).unwrap()
            })
            .collect()
    }

    #[test]
    fn base64_round_trips_every_length_up_to_sixty_four() {
        for len in 0..=64usize {
            let bytes = pseudo_random(len);
            let encoded = encode_base64_nopad(&bytes);
            assert!(
                !encoded.contains('='),
                "the no-pad encoder must not emit padding at length {len}"
            );
            assert_eq!(
                decode_base64(&encoded).unwrap(),
                bytes,
                "unpadded round trip at length {len}"
            );
            assert_eq!(
                decode_base64(&pad(&encoded)).unwrap(),
                bytes,
                "padded round trip at length {len}"
            );
        }
    }

    #[test]
    fn padding_length_follows_the_input_length_modulo_three() {
        // len % 3 == 0 -> no padding, == 1 -> two `=`, == 2 -> one `=`.
        for (len, expected_padding, expected_unpadded_len) in
            [(3usize, 0usize, 4usize), (4, 2, 6), (5, 1, 7)]
        {
            let bytes = pseudo_random(len);
            let encoded = encode_base64_nopad(&bytes);
            assert_eq!(encoded.len(), expected_unpadded_len, "length {len}");
            let padded = pad(&encoded);
            assert_eq!(
                padded.len() - encoded.len(),
                expected_padding,
                "padding for length {len}"
            );
            assert_eq!(decode_base64(&padded).unwrap(), bytes);
        }
        assert_eq!(encode_base64_nopad(b""), "");
        assert_eq!(decode_base64("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn the_decoder_rejects_bad_alphabets_and_bad_padding() {
        // Out-of-alphabet bytes, including the URL-safe alphabet, which is a
        // different encoding wearing the same shape.
        for bad in ["AAA-", "AAA_", "AA A", "AA\nA", "AAA\u{e9}", "AA*A", "AA.A"] {
            assert_eq!(decode_base64(bad), None, "must reject `{bad}`");
        }
        // Body lengths no byte string can produce, and padding that is either
        // too long, misplaced, or not on a four-character boundary.
        for bad in [
            "A", "AAAAA", "A===", "AAAA====", "AA=A", "=AAA", "AAAAA=", "AAA=A",
        ] {
            assert_eq!(decode_base64(bad), None, "must reject `{bad}`");
        }
        // The well-formed neighbours of those inputs still decode, so the test
        // proves a boundary rather than a blanket refusal.
        assert_eq!(decode_base64("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_base64("AA").unwrap(), vec![0]);
        assert_eq!(decode_base64("AAA").unwrap(), vec![0, 0]);
        assert_eq!(decode_base64("AAAA").unwrap(), vec![0, 0, 0]);
        assert_eq!(decode_base64("AA==").unwrap(), vec![0]);
        assert_eq!(decode_base64("AAA=").unwrap(), vec![0, 0]);
        assert_eq!(decode_base64("AAAAAA==").unwrap(), vec![0, 0, 0, 0]);
        assert_eq!(decode_base64("/w==").unwrap(), vec![0xff]);
        assert_eq!(decode_base64("+w==").unwrap(), vec![0xfb]);
    }

    // --- validate_authorized_keys ------------------------------------------

    fn entry(index: usize, comment: Option<&str>) -> AuthorizedKey {
        AuthorizedKey {
            key: format!("ssh-ed25519 {}", blob_for("ssh-ed25519", 64 + index * 3)),
            comment: comment.map(str::to_string),
        }
    }

    #[test]
    fn a_valid_list_is_accepted() {
        let keys = vec![
            entry(0, None),
            entry(1, Some("alice@workstation")),
            entry(2, Some("build box #2")),
        ];
        validate_authorized_keys(&keys).unwrap();
    }

    #[test]
    fn a_duplicate_key_is_rejected_and_names_its_index() {
        let mut keys = vec![entry(0, None), entry(1, None), entry(2, None)];
        keys[2].key = keys[0].key.clone();
        // The comments differ, so this is exactly the case the canonical `key`
        // exists to catch.
        keys[2].comment = Some("looks different, grants the same access".to_string());

        let err = validate_authorized_keys(&keys).unwrap_err();
        let message = message_of(&err);
        assert!(message.contains("entry 2"), "{message}");
        assert!(message.contains("entry 0"), "{message}");
    }

    #[test]
    fn the_list_is_bounded_at_thirty_two_entries() {
        let full: Vec<_> = (0..32).map(|index| entry(index, None)).collect();
        assert_eq!(full.len(), 32);
        validate_authorized_keys(&full).unwrap();

        let mut over = full;
        over.push(entry(32, None));
        let err = validate_authorized_keys(&over).unwrap_err();
        assert!(message_of(&err).contains("33 keys"), "{}", message_of(&err));
    }

    #[test]
    fn an_entry_whose_key_carries_a_comment_is_rejected() {
        let keys = vec![AuthorizedKey {
            key: valid_line("ssh-ed25519") + " smuggled",
            comment: None,
        }];
        let err = validate_authorized_keys(&keys).unwrap_err();
        assert!(
            message_of(&err).contains("carries a comment"),
            "{}",
            message_of(&err)
        );
    }

    #[test]
    fn an_entry_with_a_malformed_key_is_rejected() {
        for key in [
            "ssh-ed25519".to_string(),
            format!("ssh-dss {}", blob_for("ssh-dss", 64)),
            format!("ssh-ed25519 {}", blob_for("ssh-rsa", 64)),
            format!("command=\"x\" {}", valid_line("ssh-ed25519")),
            format!("ssh-ed25519 {}", blob_for("ssh-ed25519", 16)),
            String::new(),
        ] {
            let keys = vec![AuthorizedKey { key, comment: None }];
            let err = validate_authorized_keys(&keys).unwrap_err();
            assert!(message_of(&err).contains("entry 0"), "{}", message_of(&err));
        }
    }

    #[test]
    fn an_entry_with_a_bad_comment_field_is_rejected() {
        for comment in [
            "line\nfeed".to_string(),
            "tab\there".to_string(),
            "\u{7f}".to_string(),
            String::new(),
            "c".repeat(257),
        ] {
            let keys = vec![entry(0, Some(&comment))];
            let err = validate_authorized_keys(&keys).unwrap_err();
            assert!(message_of(&err).contains("entry 0"), "{}", message_of(&err));
        }
        // A shell metacharacter in the struct field is still fine.
        validate_authorized_keys(&[entry(0, Some("dev$box `n1` \"x\""))]).unwrap();
    }

    #[test]
    fn an_empty_list_is_accepted() {
        validate_authorized_keys(&[]).unwrap();
    }
}
