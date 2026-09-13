//! The bearer API token: its wire format, its digest, and the check that
//! decides whether a presented one is a token this device stores.
//!
//! `docs/design/api.md` section 3.2 fixes the format as `mos_<id>_<secret>`,
//! an id that is **not** secret beside a secret that is. The id is on the wire
//! for a mechanical reason: with N tokens stored, an opaque blob forces a hash
//! of the presented secret and a comparison against all N entries on every
//! request, while an embedded id is one lookup and one comparison.
//!
//! Nothing here is rate limited, and nothing here may be. The secret is 256
//! bits of `OsRng` and is not guessable online, while a shared counter on this
//! path would let anyone holding a bad token lock out every script on the
//! appliance. The login backoff (`crate::auth::GuardStore`) stays scoped to the
//! password path, which is where a human-chosen secret is.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use hmac::{Hmac, Mac};
use micad_settings::ApiToken;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::session::hex_encode;

type HmacSha256 = Hmac<Sha256>;

/// The wire format's leading field, which is what makes a leaked token
/// recognisable as one in a log or a paste.
const SCHEME: &str = "mos";

/// The separator between the format's three fields.
///
/// It is why the id and the secret are both hex: neither alphabet contains
/// this byte, so a token splits into exactly three fields with no escaping.
const SEPARATOR: char = '_';

/// Bytes of `OsRng` behind an id, hex-encoded to 8 characters.
///
/// Section 3.2's prose says "an 8-byte hex `id`" while the worked example
/// beside it -- `mos_3f2a9c41_...` -- shows eight hex CHARACTERS, which is four
/// bytes. The model refused to settle it, bounding the id to lowercase hex of
/// 1..64 characters rather than fixing a width, so both readings validate and
/// the mint has to choose. **This picks the example: four bytes, eight hex
/// characters.**
///
/// The width is not carrying uniqueness, which is the property that would
/// otherwise argue for the wider reading. [`mint`] draws against the stored
/// list and redraws on a collision, so no id is ever issued twice whatever the
/// width; the id is a lookup key and not a secret, so guessing one buys
/// nothing. What is left is the cost an operator pays: the id is the part of
/// the token a human copies into a `DELETE` path, and the document's own
/// example is the spelling a client implementer will copy first.
const ID_BYTES: usize = 4;

/// Bytes of `OsRng` behind a secret: 256 bits, double the session id's 128
/// (`crate::session`), because unlike a session this credential does not expire
/// on its own.
const SECRET_BYTES: usize = 32;

/// Redraws allowed before a mint gives up on finding a free id.
///
/// A bound rather than an unbounded loop: the stored list is capped at
/// `micad_settings::MAX_TOKENS`, so with 32 bits of id the chance of even one
/// collision is negligible and the chance of eight in a row is not a number
/// worth writing down. It exists so that a future narrower id cannot turn a
/// mint into a hang.
const MINT_ATTEMPTS: usize = 8;

/// The two fields a presented token splits into.
pub struct Presented<'a> {
    /// The stored entry this token claims to be, which is a lookup key and
    /// carries no authority of its own.
    pub id: &'a str,
    /// The half that has to hash to that entry's digest.
    pub secret: &'a str,
}

/// A newly minted token: what is stored, and the one copy of what is not.
pub struct Minted {
    /// The entry's identity, as the mint drew it.
    pub id: String,
    /// The SHA-256 hex digest, which is the only half that is stored.
    pub hash: String,
    /// The whole token, as the operator must copy it now or never.
    pub wire: String,
}

/// The `Bearer` credential of an `Authorization` header, if the request has
/// one.
///
/// The scheme is matched case-insensitively because RFC 7235 makes it so; the
/// credential after it is not touched, since a token is opaque to everything
/// but [`parse`].
pub fn bearer_from_headers(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, credential) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then(|| credential.trim())
}

/// Split a presented token into its two fields, or refuse it.
///
/// Refusing here rather than deeper is what keeps the lookup honest: a value
/// that is not this format names no entry, and a caller that reached the store
/// with one would be asking the settings tree a question about a string the
/// mint could never have produced.
pub fn parse(token: &str) -> Option<Presented<'_>> {
    let mut fields = token.split(SEPARATOR);
    let scheme = fields.next()?;
    let id = fields.next()?;
    let secret = fields.next()?;
    if fields.next().is_some() || scheme != SCHEME {
        return None;
    }
    let hex = |field: &str| {
        !field.is_empty()
            && field
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    (hex(id) && hex(secret)).then_some(Presented { id, secret })
}

/// The SHA-256 hex digest of a secret, which is what the settings tree holds.
///
/// SHA-256 and not argon2id, per section 3.2: the secret is machine-generated
/// with nothing to guess, so a work factor would buy no security and would be
/// paid on every API request rather than once per login.
///
/// The digest is taken over the secret's characters exactly as they arrive on
/// the wire, not over the bytes they decode to. Hashing the decoding would mean
/// two spellings of one secret, and the store already refuses that ambiguity
/// for the digest itself.
pub fn digest(secret: &str) -> String {
    hex_encode(&Sha256::digest(secret.as_bytes()))
}

/// Whether a presented token is one `tokens` holds.
///
/// One lookup and one comparison, which is the whole reason the id is on the
/// wire. The lookup is a plain `==` and that is deliberate: the id is not
/// secret, and section 3.2 says so as the premise of the format.
pub fn verify(tokens: &[ApiToken], presented: &str) -> bool {
    let Some(parsed) = parse(presented) else {
        return false;
    };
    let Some(entry) = tokens.iter().find(|entry| entry.id == parsed.id) else {
        return false;
    };
    digests_match(&entry.hash, &digest(parsed.secret))
}

/// Draw a token no entry of `existing` already claims.
///
/// The plaintext is returned and never stored: [`Minted::hash`] is what the
/// caller writes, and [`Minted::wire`] is the one copy that will ever exist.
///
/// `None` when every attempt collided, which is a condition no reachable tree
/// produces; see [`MINT_ATTEMPTS`].
pub fn mint(existing: &[ApiToken]) -> Option<Minted> {
    let secret = hex_encode(&random_bytes::<SECRET_BYTES>());
    let id = free_id(existing, || hex_encode(&random_bytes::<ID_BYTES>()))?;
    Some(Minted {
        hash: digest(&secret),
        wire: format!("{SCHEME}{SEPARATOR}{id}{SEPARATOR}{secret}"),
        id,
    })
}

/// The first drawn id no entry of `existing` already claims.
///
/// The draw is a parameter so the redraw has a test: with a real `OsRng` behind
/// it a collision is not an outcome a test can provoke, and an untested redraw
/// is the branch that turns out to loop forever.
fn free_id(existing: &[ApiToken], mut draw: impl FnMut() -> String) -> Option<String> {
    (0..MINT_ATTEMPTS)
        .map(|_| draw())
        .find(|id| !existing.iter().any(|entry| &entry.id == id))
}

/// `N` bytes from the operating system's entropy source.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Constant-time equality of two hex digests.
///
/// The double-HMAC comparison, under a key drawn fresh for the call: each side
/// is MACed and the two MACs are compared with `Mac::verify_slice`, which is
/// the constant-time primitive this crate already contains (`crate::session`
/// verifies a cookie signature with it). A `String` `==` on hex digests is what
/// must not be written here.
///
/// The honest size of the requirement, since overstating it would be its own
/// error: a timing leak on a *stored digest* is not a practical attack, because
/// learning a digest yields no preimage. It is constant-time anyway, because
/// deciding this site by site is how the one site where it matters gets missed.
fn digests_match(left: &str, right: &str) -> bool {
    let key = random_bytes::<32>();
    let mac = |value: &str| {
        let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
        mac.update(value.as_bytes());
        mac
    };
    mac(right)
        .verify_slice(&mac(left).finalize().into_bytes())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value.parse().expect("a header value"));
        headers
    }

    fn stored(minted: &Minted, name: &str) -> ApiToken {
        ApiToken {
            id: minted.id.clone(),
            name: name.to_string(),
            hash: minted.hash.clone(),
            created: 0,
        }
    }

    /// The mint's own output is the thing the check has to accept, and the
    /// shape of it is the document's worked example: `mos_<8 hex>_<64 hex>`.
    #[test]
    fn a_minted_token_has_the_documented_shape_and_verifies() {
        let minted = mint(&[]).expect("an empty list leaves every id free");
        let parsed = parse(&minted.wire).expect("the mint emits the format it parses");

        assert_eq!(parsed.id.len(), ID_BYTES * 2);
        assert_eq!(parsed.secret.len(), SECRET_BYTES * 2);
        assert_eq!(parsed.id, minted.id);
        assert_eq!(minted.wire, format!("mos_{}_{}", parsed.id, parsed.secret));
        assert!(micad_settings::is_api_token_id(&minted.id));

        assert!(verify(&[stored(&minted, "ci-deploy")], &minted.wire));
    }

    /// Two mints differ in both halves, which is what makes the id an identity
    /// and the secret a credential.
    #[test]
    fn two_mints_share_neither_field() {
        let first = mint(&[]).unwrap();
        let second = mint(&[stored(&first, "first")]).unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(first.wire, second.wire);
        assert_ne!(first.hash, second.hash);
    }

    /// The redraw, driven by a scripted draw because `OsRng` cannot be made to
    /// collide on demand: the first two answers are ids already held and the
    /// third is free, and the free one is what comes back.
    #[test]
    fn a_collided_id_is_redrawn_and_an_exhausted_draw_refuses() {
        let taken = ["3f2a9c41", "0000dead"];
        let existing: Vec<ApiToken> = taken
            .iter()
            .enumerate()
            .map(|(index, id)| ApiToken {
                id: (*id).to_string(),
                name: format!("held-{index}"),
                hash: format!("{index:064x}"),
                created: 0,
            })
            .collect();

        let mut drawn = ["3f2a9c41", "0000dead", "0000beef"].into_iter();
        assert_eq!(
            free_id(&existing, || drawn.next().expect("three draws").to_string()),
            Some("0000beef".to_string())
        );

        // A draw that never frees gives up rather than spinning.
        let mut attempts = 0;
        let exhausted = free_id(&existing, || {
            attempts += 1;
            taken[0].to_string()
        });
        assert_eq!(exhausted, None);
        assert_eq!(attempts, MINT_ATTEMPTS);
    }

    /// The digest is over the secret's characters and matches the store's
    /// grammar for the field it goes into.
    #[test]
    fn the_digest_is_lowercase_hex_of_the_secret_as_sent() {
        let value = digest("abc");
        assert_eq!(
            value,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(value.len(), 64);
        assert_eq!(value, value.to_lowercase());
    }

    /// Everything that is not the format names no entry, and the refusal is at
    /// the parser rather than at a lookup with a nonsense key.
    #[test]
    fn nothing_but_the_format_parses() {
        for value in [
            "",
            "mos",
            "mos_",
            "mos_3f2a9c41",
            "mos_3f2a9c41_",
            "_3f2a9c41_9d4e",
            "mos_3f2a9c41_9d4e_extra",
            "sess_3f2a9c41_9d4e",
            "MOS_3f2a9c41_9d4e",
            // Uppercase hex is refused rather than folded, for the reason the
            // store refuses it in the digest: one value must have one spelling.
            "mos_3F2A9C41_9d4e",
            "mos_3f2a9c41_9D4E",
            "mos_ci-deploy_9d4e",
            "mos_3f2a9c41_zzzz",
        ] {
            assert!(parse(value).is_none(), "{value:?} parsed");
        }
    }

    /// A token whose id names nothing, and one whose id names an entry its
    /// secret does not match, are both refused -- the second being the case a
    /// lookup alone would pass.
    #[test]
    fn a_wrong_id_and_a_wrong_secret_are_both_refused() {
        let minted = mint(&[]).unwrap();
        let entry = stored(&minted, "ci-deploy");

        assert!(!verify(&[], &minted.wire), "an empty list matches nothing");

        let other = mint(std::slice::from_ref(&entry)).unwrap();
        assert!(!verify(std::slice::from_ref(&entry), &other.wire));

        // The stored entry's own id, carrying somebody else's secret.
        let forged = format!("mos_{}_{}", entry.id, parse(&other.wire).unwrap().secret);
        assert!(!verify(&[entry], &forged));
    }

    /// The comparison answers equality, which a constant-time primitive can
    /// silently stop doing.
    #[test]
    fn the_constant_time_compare_is_still_an_equality() {
        let value = digest("abc");
        assert!(digests_match(&value, &value));
        assert!(!digests_match(&value, &digest("abd")));
        assert!(!digests_match(&value, ""));
        assert!(!digests_match(&value, &value[..63]));
    }

    #[test]
    fn the_bearer_scheme_is_read_case_insensitively_and_nothing_else_is_read() {
        assert_eq!(
            bearer_from_headers(&header("Bearer mos_a_b")),
            Some("mos_a_b")
        );
        assert_eq!(
            bearer_from_headers(&header("bearer mos_a_b")),
            Some("mos_a_b")
        );
        assert_eq!(
            bearer_from_headers(&header("BEARER mos_a_b")),
            Some("mos_a_b")
        );
        assert_eq!(bearer_from_headers(&header("Basic mos_a_b")), None);
        assert_eq!(bearer_from_headers(&header("mos_a_b")), None);
        assert_eq!(bearer_from_headers(&HeaderMap::new()), None);
    }
}
