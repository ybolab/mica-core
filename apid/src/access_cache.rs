//! The auth gate's cache of the `access` settings subtree.
//!
//! Every request the gate cannot short-circuit on a session cookie needs the
//! `access` subtree to decide between setup mode and a login redirect, and it
//! used to fetch it with a `GetSettings` round trip per request — against the
//! one mutex mosd holds over both trees, on a listener that is public by
//! construction. This cache serves that read from apid's memory instead, so
//! unauthenticated traffic stops contending with settings writes.
//!
//! # The lockout rule
//!
//! A stale setup-mode decision is a lockout: a gate that still believes no
//! admin password exists sends everyone to `/setup`, and one that still
//! believes a deleted credential exists locks the recovery surface. So the
//! cache **never serves unless it is provably fresh**:
//!
//! - It serves only while a `SettingsChanged` subscription is known live
//!   ([`AccessCache::subscribed`] .. [`AccessCache::lapsed`]); constructed
//!   unsynchronised, and any doubt — stream end, connection error — drops it
//!   back to the per-request direct read the gate always did.
//! - A relevant signal, and every access write apid makes itself, drops the
//!   value ([`AccessCache::invalidate`]); the next request re-reads.
//! - A fill is generation-checked: the gate snapshots
//!   [`AccessCache::generation`] before its direct read, and a fill whose
//!   read began before an invalidation or a subscription transition is
//!   discarded, so a change signalled while the read was in flight can never
//!   be papered over by that read's stale result.

use std::sync::{Mutex, PoisonError};

use serde_json::Value;

/// The dot-path this cache holds, and the subtree the signal filter watches.
pub const ACCESS_PATH: &str = "access";

/// Whether a `SettingsChanged` at `path` can affect the `access` subtree: the
/// whole tree, `access` itself, or anything under it. Segment-wise on
/// purpose — `accessory` must not match.
pub fn touches_access(path: &str) -> bool {
    path.is_empty()
        || path == "."
        || path == ACCESS_PATH
        || path
            .strip_prefix(ACCESS_PATH)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[derive(Default)]
struct Inner {
    /// True only while a `SettingsChanged` subscription is known to be live.
    synchronised: bool,
    /// Bumped on every invalidation and subscription transition, so a fill
    /// whose read began before either is recognisably stale.
    generation: u64,
    value: Option<Value>,
}

/// See the module docs.
#[derive(Default)]
pub struct AccessCache {
    inner: Mutex<Inner>,
}

impl AccessCache {
    /// An empty, unsynchronised cache: every [`Self::get`] answers `None`
    /// until a subscription is live and a fill lands.
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock, recovered from poisoning rather than propagated: every
    /// mutation below is a plain field write that cannot leave the state
    /// half-applied, and a panic elsewhere must not convert the auth gate
    /// into a permanent 500.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached subtree, or `None` when the gate must read directly:
    /// nothing cached yet, or the subscription is not known live.
    pub fn get(&self) -> Option<Value> {
        let inner = self.lock();
        if inner.synchronised {
            inner.value.clone()
        } else {
            None
        }
    }

    /// The snapshot to pass to [`Self::fill`], taken BEFORE the direct read.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Store a directly-read value — unless the world moved since
    /// `generation` was taken, or no subscription is live to keep it honest.
    pub fn fill(&self, generation: u64, value: Value) {
        let mut inner = self.lock();
        if inner.synchronised && inner.generation == generation {
            inner.value = Some(value);
        }
    }

    /// Drop the cached value: a relevant `SettingsChanged` arrived, or apid
    /// itself just wrote under `access` and is not waiting for the signal's
    /// round trip to tell it so.
    pub fn invalidate(&self) {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.value = None;
    }

    /// A `SettingsChanged` subscription is live from here on. The value
    /// starts empty — anything cached before or across a subscription gap is
    /// of unknown age.
    pub fn subscribed(&self) {
        let mut inner = self.lock();
        inner.synchronised = true;
        inner.generation += 1;
        inner.value = None;
    }

    /// The subscription lapsed: back to per-request direct reads until a new
    /// one is live. This is the fallback the lockout rule demands.
    pub fn lapsed(&self) {
        let mut inner = self.lock();
        inner.synchronised = false;
        inner.generation += 1;
        inner.value = None;
    }

    /// Whether a subscription is currently marked live; for the tests that
    /// wait on the watcher's state transitions.
    #[cfg(test)]
    pub fn is_synchronised(&self) -> bool {
        self.lock().synchronised
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AccessCache, touches_access};

    /// Segment-wise in both directions, and never on a sibling prefix.
    #[test]
    fn the_signal_filter_matches_by_dot_segments() {
        assert!(touches_access(""));
        assert!(touches_access("."));
        assert!(touches_access("access"));
        assert!(touches_access("access.webAdmin"));
        assert!(touches_access("access.webAdmin.password_hash"));
        assert!(!touches_access("accessory"));
        assert!(!touches_access("hostname"));
        assert!(!touches_access("network.eth0"));
    }

    /// The lockout rule's floor: with no live subscription nothing is served
    /// and nothing is stored, whatever was filled.
    #[test]
    fn an_unsynchronised_cache_serves_and_stores_nothing() {
        let cache = AccessCache::new();
        assert_eq!(cache.get(), None);
        cache.fill(cache.generation(), json!({"webAdmin": {}}));
        assert_eq!(cache.get(), None);
    }

    /// The happy path: subscribed, filled, served; invalidated, empty.
    #[test]
    fn a_synchronised_cache_serves_a_fill_until_invalidated() {
        let cache = AccessCache::new();
        cache.subscribed();
        let generation = cache.generation();
        cache.fill(generation, json!({"webAdmin": {}}));
        assert_eq!(cache.get(), Some(json!({"webAdmin": {}})));
        cache.invalidate();
        assert_eq!(cache.get(), None);
    }

    /// The in-flight race: a change signalled between the generation snapshot
    /// and the fill discards the fill, because that read may predate the
    /// change it lost to.
    #[test]
    fn a_fill_that_lost_a_race_to_an_invalidation_is_discarded() {
        let cache = AccessCache::new();
        cache.subscribed();
        let generation = cache.generation();
        cache.invalidate();
        cache.fill(generation, json!({"stale": true}));
        assert_eq!(cache.get(), None);
    }

    /// A lapse empties the cache and disables serving until resubscribed, and
    /// resubscribing does not resurrect the pre-gap value.
    #[test]
    fn a_lapse_disables_serving_and_a_resubscribe_starts_empty() {
        let cache = AccessCache::new();
        cache.subscribed();
        cache.fill(cache.generation(), json!({"webAdmin": {}}));
        cache.lapsed();
        assert_eq!(cache.get(), None);
        cache.subscribed();
        assert_eq!(cache.get(), None, "a value from before the gap came back");
    }
}
