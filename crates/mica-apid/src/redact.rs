//! The structural redactor every value leaving §2.2's two read-only roots
//! passes through.
//!
//! `docs/design/api.md` §2.2 states the rule for the settings root, where
//! `GetSettings("access")` would otherwise hand the admin password hash to any
//! authenticated caller. It is applied to the live-state root as well: micad's
//! state tree is untyped and written by reconcilers, so nothing stops the same
//! field names appearing there, and a denylist that covers one root while the
//! other serves them verbatim is a hole with a tested-looking lid.
//!
//! The list is fail-open: a secret-bearing field under a name it does not
//! carry is served. §2.2 says the mitigation is a test rather than a hope, and
//! `every_redacted_field_name_comes_back_redacted_from_the_settings_root` is
//! that test.

use serde_json::Value;

/// What a redacted field carries in place of its value.
pub const REDACTED: &str = "<redacted>";

/// The field names whose values never leave the device (§2.2).
///
/// Names and not dot-paths, because the two `psk` fields sit inside arrays and
/// the dot-path syntax cannot name an array element.
///
/// `privateKey` is on the list for a value nothing in this tree serves. A
/// WireGuard private key is not in the settings schema and is not in live
/// state — it lives in a mode-0640 file on STATE and
/// states *"there is no read-back route for the private key, ever"* — so today
/// this entry redacts nothing. It is the fail-closed half of that rule: the day
/// a field of that name appears anywhere in either tree, it is already covered,
/// rather than being served in the clear until somebody remembers this file.
///
/// `hash` was on the list before any settings field carried that name, and it
/// is the case that proves the paragraph above. `access.apiTokens[].hash` is
/// now a real field of the settings schema -- the SHA-256 digest of a bearer
/// token -- and it arrived already covered, because `GetSettings("access")`
/// returns the subtree verbatim and the mint pane would otherwise have shipped
/// a digest to every authenticated reader before anyone thought to look here.
///
/// **The rule that makes that luck into a rule** (PLAN-070 section 5.2.4.2).
/// This list is a denylist of field names and it is fail-open by design, so
/// now that `/mica/config/` holds secrets on purpose:
///
/// > A `/mica/config/` document spells a secret-bearing key with a name already
/// > on this list, or the same change that adds the key adds the name.
///
/// Stated because the alternative is a subsystem author who picks
/// `sharedSecret`, ships it, and finds out from a support case. The moved
/// schema satisfies it today with nothing added: the only secret-bearing keys
/// in the namespace are `wifi.ap.psk` and `wifi.client.networks[].psk`, both
/// spelled `psk`. `mqtt.auth` carries no credential at all -- the broker reads
/// its accounts from a STATE file, the rule stated at that field.
const SECRET_FIELDS: [&str; 5] = ["psk", "passwordHash", "password_hash", "hash", "privateKey"];

/// Whether a field named `name` is redacted.
fn is_secret(name: &str) -> bool {
    SECRET_FIELDS.contains(&name)
}

/// `value`, read from dot-path `path`, with every secret it carries replaced.
///
/// `path` is read as well as the value, because a dot-path can name a secret
/// field directly: `GET /api/v1/settings/access.webAdmin.password_hash`
/// answers the hash as a bare JSON string, and a bare string has no field name
/// left in it for the structural walk below to key on.
pub fn redact(value: Value, path: &str) -> Value {
    let leaf = path.rsplit('.').next().unwrap_or_default();
    if is_secret(leaf) {
        return Value::String(REDACTED.to_string());
    }
    walk(value)
}

/// `value` with the value of every secret-bearing field replaced by
/// [`REDACTED`], at any depth and inside arrays.
fn walk(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, child)| {
                    let child = if is_secret(&name) {
                        Value::String(REDACTED.to_string())
                    } else {
                        walk(child)
                    };
                    (name, child)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(walk).collect()),
        other => other,
    }
}

/// Whether `value` carries [`REDACTED`] anywhere inside it.
///
/// The structural mirror of [`walk`], and it exists for one round trip §2.2
/// names: *"A redacted field is **read-only through the API**: a `PUT` whose
/// body contains `"<redacted>"` is rejected at 422 rather than written,
/// because writing the sentinel would silently destroy the credential."*
/// (`docs/design/api.md` §2.2) A client that reads a subtree, edits one
/// field and writes the whole thing back is not doing anything unusual; what
/// it hands back is the redacted view, and without this check the write
/// succeeds and the password hash on the device becomes the literal string
/// `<redacted>`.
///
/// Strings at any depth and inside arrays, because that is exactly where
/// [`walk`] puts the sentinel. Field *names* are not examined: a read
/// substitutes values and never renames a field, so a key that happens to
/// spell the sentinel did not come from one.
///
/// The cost is named rather than hidden: an operator who genuinely wants a
/// setting whose value is the ten characters `<redacted>` cannot write it
/// through this route. No such setting exists in the schema — every field it
/// would reach is a hostname or a boolean — and the alternative is a rule that
/// cannot tell the destructive case from the deliberate one.
pub fn carries_sentinel(value: &Value) -> bool {
    match value {
        Value::String(text) => text == REDACTED,
        Value::Object(fields) => fields.values().any(carries_sentinel),
        Value::Array(items) => items.iter().any(carries_sentinel),
        _ => false,
    }
}
