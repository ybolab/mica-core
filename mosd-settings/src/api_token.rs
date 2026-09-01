//! Validator for the bearer API token list.
//!
//! This module is a security boundary in the same sense
//! [`crate::validate_authorized_keys`] is, and in a narrower one: nothing here
//! decides whether a presented token is accepted -- that is apid's bearer
//! check -- but everything here decides whether the list apid looks a token up
//! in can answer unambiguously. A duplicate id makes a lookup ambiguous, a
//! duplicate digest makes a revocation lie, and a malformed digest makes an
//! entry that can never match anything while looking to an operator exactly
//! like one that can.
//!
//! What is deliberately NOT checked is `created`. See [`crate::ApiToken`]:
//! this tree has no trusted wall clock, so a bound on that field would let a
//! wrong clock decide whether the operator may hold a credential.

use std::collections::BTreeMap;

use crate::error::SettingsError;
use crate::model::ApiToken;

/// Dot-path reported by every error raised here.
const TOKENS_PATH: &str = "access.apiTokens";

/// Largest number of tokens the settings tree will hold.
///
/// A bound has to exist, and this one is tighter than a STATE-growth argument
/// alone would need: apid's gate reads the whole `access` subtree on every
/// request, so the list is on the hot path of every API call. 32 matches
/// `authorized_keys`' own cap for the same reason it was chosen there -- far
/// past any real appliance, far short of a list an operator could use to fill
/// STATE.
pub const MAX_TOKENS: usize = 32;

/// Largest token name accepted, in bytes. The `authorized_keys` comment bound,
/// for the same field: an operator label with no other job.
const MAX_NAME_BYTES: usize = 256;

/// Length of a SHA-256 digest in lowercase hex.
const HASH_HEX_LEN: usize = 64;

/// Largest id accepted, in characters.
///
/// A ceiling and not a fixed width, deliberately. The mint's id width is a
/// property of the token wire format, which this crate does not own; pinning
/// it here would make a change to that format a schema change. What the model
/// does require is that an id is short enough to sit in a `DELETE` path
/// segment and narrow enough to survive one.
const MAX_ID_CHARS: usize = 64;

/// Reject a token list the store must not hold.
///
/// Callers validate before writing, exactly as they do for authorized keys:
/// the check is not wired into [`crate::Settings::set`] because the tree is a
/// file anything with STATE write access can edit, so the boundary belongs at
/// the write path that has an operator behind it.
///
/// # Errors
///
/// Returns [`SettingsError::Validation`] naming `access.apiTokens`, the index
/// of the offending entry, and what is wrong with it. No `hash` and no `id` is
/// echoed back into the message.
pub fn validate_api_tokens(tokens: &[ApiToken]) -> Result<(), SettingsError> {
    if tokens.len() > MAX_TOKENS {
        return Err(invalid(format!(
            "list holds {} tokens, above the maximum of {MAX_TOKENS}",
            tokens.len()
        )));
    }
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    let mut hashes: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, entry) in tokens.iter().enumerate() {
        check_id(&entry.id).map_err(|message| invalid(format!("entry {index}: {message}")))?;
        check_name(&entry.name).map_err(|message| invalid(format!("entry {index}: {message}")))?;
        check_hash(&entry.hash).map_err(|message| invalid(format!("entry {index}: {message}")))?;
        if let Some(first) = ids.insert(entry.id.as_str(), index) {
            return Err(invalid(format!(
                "entry {index} carries the id already held by entry {first}, so a lookup by id has two answers"
            )));
        }
        if let Some(first) = hashes.insert(entry.hash.as_str(), index) {
            return Err(invalid(format!(
                "entry {index} carries the digest already held by entry {first}, so revoking either leaves the secret working"
            )));
        }
    }
    Ok(())
}

/// Whether `id` is well formed, without saying whether any entry carries it.
///
/// The grammar and the lookup are different questions, and a route that
/// answers them with one check answers the wrong one: an item route owes a
/// **404** for an identifier that is well formed and names nothing and a
/// **422** for one that is not well formed at all (the contract
/// section 2.4). It cannot tell those apart without asking this separately,
/// and a second copy of the grammar at the route could disagree with the copy
/// the store enforces.
pub fn is_api_token_id(id: &str) -> bool {
    check_id(id).is_ok()
}

/// Reject an id that cannot be a stable, addressable identity.
///
/// Lowercase hex is the narrowest grammar that serves both places the id is
/// spelled: a `DELETE /api/v1/tokens/{id}` path segment, which must not need
/// escaping, and the `_`-separated token wire format, which must not have a
/// separator inside a field.
fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id is empty".to_string());
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Err(format!(
            "id is longer than the maximum of {MAX_ID_CHARS} characters"
        ));
    }
    if !id.bytes().all(is_lowercase_hex) {
        return Err("id is not lowercase hex".to_string());
    }
    Ok(())
}

/// Reject a name an operator could not read a listing by.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(
            "name is empty; it is the only thing that tells one token from another in a listing"
                .to_string(),
        );
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "name is {} bytes, above the maximum of {MAX_NAME_BYTES}",
            name.len()
        ));
    }
    for (offset, ch) in name.char_indices() {
        if (ch as u32) < 0x20 || ch == '\u{7f}' {
            return Err(format!("name holds a control character at byte {offset}"));
        }
    }
    Ok(())
}

/// Reject anything that is not a lowercase-hex SHA-256 digest.
///
/// Uppercase hex is rejected rather than folded: apid compares digests, and two
/// spellings of one digest is a comparison that depends on which code path
/// wrote the entry.
fn check_hash(hash: &str) -> Result<(), String> {
    if hash.len() != HASH_HEX_LEN {
        return Err(format!(
            "hash is {} characters, not the {HASH_HEX_LEN} of a SHA-256 hex digest",
            hash.len()
        ));
    }
    if !hash.bytes().all(is_lowercase_hex) {
        return Err("hash is not lowercase hex".to_string());
    }
    Ok(())
}

/// Whether `byte` is a lowercase hex digit.
fn is_lowercase_hex(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}

/// Build the one error variant this module raises.
fn invalid(message: impl Into<String>) -> SettingsError {
    SettingsError::Validation {
        path: TOKENS_PATH.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed entry, varied by `seed` so two calls differ in both the
    /// fields identity is keyed on.
    fn token(seed: u8) -> ApiToken {
        ApiToken {
            id: format!("{seed:016x}"),
            name: format!("ci-deploy-{seed}"),
            hash: format!("{seed:064x}"),
            created: 1_700_000_000,
        }
    }

    /// The message every rejection here carries, so a caller can report the
    /// path without knowing which check fired.
    fn message(err: SettingsError) -> String {
        match err {
            SettingsError::Validation { path, message } => {
                assert_eq!(path, "access.apiTokens");
                message
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_list_and_a_well_formed_list_are_accepted() {
        validate_api_tokens(&[]).unwrap();
        validate_api_tokens(&[token(1), token(2), token(3)]).unwrap();
    }

    /// Identity is the id, so two entries claiming one id make `DELETE
    /// /api/v1/tokens/{id}` a question with two answers.
    #[test]
    fn two_entries_may_not_share_an_id() {
        let mut second = token(2);
        second.id = token(1).id;

        let text = message(validate_api_tokens(&[token(1), second]).unwrap_err());

        assert!(text.contains("entry 1"), "{text}");
        assert!(text.contains("entry 0"), "{text}");
        assert!(text.contains("two answers"), "{text}");
    }

    /// Two names for one secret makes revocation lie: revoking one entry
    /// leaves the other matching the same presented token.
    #[test]
    fn two_entries_may_not_share_a_digest() {
        let mut second = token(2);
        second.hash = token(1).hash;

        let text = message(validate_api_tokens(&[token(1), second]).unwrap_err());

        assert!(text.contains("digest"), "{text}");
        assert!(text.contains("leaves the secret working"), "{text}");
    }

    /// A digest of the wrong length or the wrong alphabet can never match a
    /// presented token, so it is a dead entry that looks alive.
    #[test]
    fn a_malformed_digest_is_refused() {
        for (hash, expected) in [
            (String::new(), "0 characters"),
            ("abc".to_string(), "3 characters"),
            ("a".repeat(63), "63 characters"),
            ("a".repeat(65), "65 characters"),
            ("A".repeat(64), "not lowercase hex"),
            ("g".repeat(64), "not lowercase hex"),
            ("z".repeat(64), "not lowercase hex"),
        ] {
            let mut entry = token(1);
            entry.hash = hash.clone();
            let text = message(validate_api_tokens(&[entry]).unwrap_err());
            assert!(text.contains(expected), "{hash:?} gave {text}");
            assert!(text.starts_with("entry 0: "), "{text}");
        }
    }

    /// The rejected digest is never echoed back: an error message is a place
    /// a stored credential must not leak into.
    #[test]
    fn a_rejection_does_not_echo_the_digest_or_the_id() {
        let mut entry = token(1);
        entry.hash = "DEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF".to_string();

        let text = message(validate_api_tokens(&[entry.clone()]).unwrap_err());

        assert!(!text.contains(&entry.hash), "{text}");
        assert!(!text.contains(&entry.id), "{text}");
    }

    #[test]
    fn a_malformed_id_is_refused() {
        for (id, expected) in [
            (String::new(), "id is empty"),
            ("3F2A9C41".to_string(), "not lowercase hex"),
            ("ci-deploy".to_string(), "not lowercase hex"),
            ("3f2a_9c41".to_string(), "not lowercase hex"),
            ("3f2a/9c41".to_string(), "not lowercase hex"),
            ("a".repeat(MAX_ID_CHARS + 1), "longer than the maximum"),
        ] {
            let mut entry = token(1);
            entry.id = id.clone();
            let text = message(validate_api_tokens(&[entry]).unwrap_err());
            assert!(text.contains(expected), "{id:?} gave {text}");
        }
    }

    #[test]
    fn a_name_that_names_nothing_is_refused() {
        for (name, expected) in [
            (String::new(), "name is empty"),
            ("a".repeat(MAX_NAME_BYTES + 1), "above the maximum"),
            ("ci\ndeploy".to_string(), "control character at byte 2"),
            ("ci\u{7f}deploy".to_string(), "control character at byte 2"),
        ] {
            let mut entry = token(1);
            entry.name = name.clone();
            let text = message(validate_api_tokens(&[entry]).unwrap_err());
            assert!(text.contains(expected), "{name:?} gave {text}");
        }
    }

    /// The cap is on the list, and the message says what it found rather than
    /// only what it wanted.
    #[test]
    fn a_list_above_the_cap_is_refused_and_the_cap_itself_is_accepted() {
        let full: Vec<ApiToken> = (0..MAX_TOKENS)
            .map(|index| token(u8::try_from(index).unwrap()))
            .collect();
        validate_api_tokens(&full).unwrap();

        let mut over = full;
        over.push(token(u8::try_from(MAX_TOKENS).unwrap()));
        let text = message(validate_api_tokens(&over).unwrap_err());
        assert!(text.contains("33 tokens"), "{text}");
        assert!(text.contains("maximum of 32"), "{text}");
    }

    /// `created` is a clock reading from a device with no time source, so no
    /// value of it is a reason to refuse a credential -- 0, the epoch, and a
    /// year the device cannot have reached all pass.
    #[test]
    fn no_value_of_created_is_rejected() {
        for created in [0, 1, 1_700_000_000, u64::MAX] {
            let mut entry = token(1);
            entry.created = created;
            validate_api_tokens(&[entry]).unwrap();
        }
    }
}
