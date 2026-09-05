//! HMAC-signed session cookies backed by an in-process session table.
//!
//! A cookie value is `<id>.<mac>` where `id` is 16 random bytes hex-encoded
//! (128 bits) and `mac` is the hex HMAC-SHA256 of `id` under the persistent
//! signing key. Sessions live in memory only and expire after 24 hours, so an
//! apid restart logs everyone out.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

/// Session cookie name.
pub const COOKIE_NAME: &str = "apid_session";
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

type HmacSha256 = Hmac<Sha256>;

/// Signed-cookie session table.
///
/// Lock acquisitions recover from poisoning rather than propagating it: the
/// table is a plain map with no invariant a mid-update panic could half
/// establish, and treating poison as fatal would convert one panic while
/// holding the lock into a panic on every later login and session check — a
/// permanent denial of management out of a transient bug.
pub struct SessionStore {
    key: [u8; 32],
    sessions: Mutex<HashMap<String, StoredSession>>,
    generation: AtomicU64,
}

struct StoredSession {
    expires_at: Instant,
    csrf_token: String,
}

/// Credentials created for one authenticated browser session.
pub struct CreatedSession {
    /// The signed value stored in the HttpOnly cookie.
    pub cookie: String,
    /// The request token the SPA sends on state-changing API calls.
    pub csrf_token: String,
}

impl SessionStore {
    /// Store signing cookies with `key`.
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            sessions: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    fn mac(&self, id: &str) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(id.as_bytes());
        mac
    }

    /// Create a session and return its cookie and CSRF credentials.
    pub fn create(&self) -> CreatedSession {
        self.create_in(
            &mut self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Capture before reading the password used to authenticate a login.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Issue only if no credential rotation completed during authentication.
    pub fn create_if_current(&self, generation: u64) -> Option<CreatedSession> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.generation() == generation).then(|| self.create_in(&mut sessions))
    }

    fn create_in(&self, sessions: &mut HashMap<String, StoredSession>) -> CreatedSession {
        let mut id_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id_bytes);
        let id = hex_encode(&id_bytes);
        let mut csrf_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut csrf_bytes);
        let csrf_token = hex_encode(&csrf_bytes);
        let mac = hex_encode(&self.mac(&id).finalize().into_bytes());
        sessions.insert(
            id.clone(),
            StoredSession {
                expires_at: Instant::now() + SESSION_TTL,
                csrf_token: csrf_token.clone(),
            },
        );
        CreatedSession {
            cookie: format!("{id}.{mac}"),
            csrf_token,
        }
    }

    /// Split a cookie value into its id, verifying the signature.
    fn verify_signature(&self, value: &str) -> Option<String> {
        let (id, mac_hex) = value.split_once('.')?;
        let mac_bytes = hex_decode(mac_hex)?;
        self.mac(id).verify_slice(&mac_bytes).ok()?;
        Some(id.to_string())
    }

    /// True when `value` is well-signed and names a live session.
    pub fn verify(&self, value: &str) -> bool {
        let Some(id) = self.verify_signature(value) else {
            return false;
        };
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match sessions.get(&id) {
            Some(session) if session.expires_at > Instant::now() => true,
            Some(_) => {
                sessions.remove(&id);
                false
            }
            None => false,
        }
    }

    /// Return the CSRF token for a live, signed session cookie.
    pub fn csrf_token(&self, value: &str) -> Option<String> {
        let id = self.verify_signature(value)?;
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match sessions.get(&id) {
            Some(session) if session.expires_at > Instant::now() => {
                Some(session.csrf_token.clone())
            }
            Some(_) => {
                sessions.remove(&id);
                None
            }
            None => None,
        }
    }

    /// Verify a CSRF token without an early-exit string comparison.
    pub fn verify_csrf(&self, value: &str, presented: &str) -> bool {
        let Some(expected) = self.csrf_token(value) else {
            return false;
        };
        let expected_mac = self.mac(&expected).finalize().into_bytes();
        self.mac(presented).verify_slice(&expected_mac).is_ok()
    }

    /// Drop every session except the one named by `value`.
    ///
    /// The password-change path calls this: every other session was minted
    /// under the old credential, and the one performing the change is the one
    /// proof of possession the new credential has. A `value` that does not
    /// verify keeps nothing, which errs closed.
    pub fn remove_all_except(&self, value: &str) {
        let keep = self.verify_signature(value);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The check and insertion in create_if_current take the same lock.
        // A login either predates this removal or sees the new generation.
        self.generation.fetch_add(1, Ordering::Release);
        match keep {
            Some(id) => sessions.retain(|key, _| *key == id),
            None => sessions.clear(),
        }
    }

    /// Drop the session named by `value`, if any.
    pub fn remove(&self, value: &str) {
        if let Some(id) = self.verify_signature(value) {
            self.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
        }
    }
}

/// `Set-Cookie` value establishing a session.
pub fn session_cookie(value: &str) -> String {
    format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=86400")
}

/// `Set-Cookie` value clearing the session cookie.
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// Extract the session cookie value from request headers.
pub fn cookie_from_headers(headers: &HeaderMap) -> Option<String> {
    let prefix = format!("{COOKIE_NAME}=");
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(text) = header.to_str() else { continue };
        for pair in text.split(';') {
            if let Some(value) = pair.trim().strip_prefix(&prefix) {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper() {
        let store = SessionStore::new([1u8; 32]);
        let session = store.create();
        let value = session.cookie;
        assert!(store.verify(&value));
        assert!(store.verify_csrf(&value, &session.csrf_token));
        assert!(!store.verify_csrf(&value, "wrong"));

        let mut tampered = value.clone().into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'a' { b'b' } else { b'a' };
        assert!(!store.verify(&String::from_utf8(tampered).unwrap()));

        store.remove(&value);
        assert!(!store.verify(&value));
    }

    #[test]
    fn remove_all_except_keeps_only_the_named_session() {
        let store = SessionStore::new([1u8; 32]);
        let kept = store.create().cookie;
        let dropped = store.create().cookie;
        store.remove_all_except(&kept);
        assert!(store.verify(&kept));
        assert!(!store.verify(&dropped));

        // A value that does not verify keeps nothing.
        store.remove_all_except("not-a-cookie");
        assert!(!store.verify(&kept));
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x1a]), "00ff1a");
        assert_eq!(hex_decode("00ff1a"), Some(vec![0x00, 0xff, 0x1a]));
        assert_eq!(hex_decode("0g"), None);
        assert_eq!(hex_decode("abc"), None);
    }
}
