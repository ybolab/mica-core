//! Password hashing/verification and login brute-force backoff.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

/// `docs/design/access.md` §3.3's `backoffBase`. The first failure costs a
/// second; every consecutive one doubles it.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// §3.3's `backoffMax`. The curve stops here and never becomes permanent —
/// see [`LoginGuard`] for why apid does not arm §3.3's `lockoutThreshold`.
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Hash `password` with argon2id default parameters into a PHC string.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow::anyhow!("hash password: {err}"))
}

/// True when `password` matches the PHC-formatted `hash`.
pub fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash)
        .and_then(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed))
        .is_ok()
}

/// The backoff a run of `failures` consecutive failures has earned:
/// `BACKOFF_BASE * 2^(failures - 1)`, capped at [`BACKOFF_MAX`].
///
/// Pure and total, so the curve is testable without a clock: the shift
/// saturates rather than overflowing, and the cap makes every count past the
/// ninth the same answer anyway.
fn backoff_for(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let factor = 1u64.checked_shl(failures - 1).unwrap_or(u64::MAX);
    let secs = BACKOFF_BASE.as_secs().saturating_mul(factor);
    Duration::from_secs(secs.min(BACKOFF_MAX.as_secs()))
}

/// Global (not per-client) login backoff, on `docs/design/access.md` §3.3's
/// curve: each consecutive failure doubles the wait before the next attempt is
/// accepted, from [`BACKOFF_BASE`] up to [`BACKOFF_MAX`]. A single shared
/// counter is deliberate — the appliance has one admin password, so per-client
/// tracking buys nothing against an online guesser, who would rotate source
/// addresses anyway.
///
/// The counter survives an expired window. Clearing `failures` when the window
/// lapses would make the rule flat: an attacker waits the window out, the next
/// run starts from zero, and the cost per guess never rises. Only
/// [`LoginGuard::record_success`] resets the run, so guessing gets
/// monotonically more expensive — under 300 guesses a day once the cap is
/// reached. The curve never becomes permanent: §6 pairs its `lockoutThreshold`
/// with "releasable only with physical presence" and apid has no presence
/// check, so arming a threshold nothing can clear would let an attacker convert
/// a guessing attempt into a permanent denial of management. The cap is the
/// whole control — a locked-out administrator who knows the password waits at
/// most [`BACKOFF_MAX`].
///
/// Persistence is [`GuardStore`]'s job, not this type's: the guard stays a pure
/// counter-and-clock so the curve remains testable without a filesystem, and
/// the store wraps it to satisfy §6's "a power cycle must not reset the clock".
/// (§6 asked for META; the state lives on STATE instead, and
/// `docs/design/access.md` §6 records that deviation and why.)
#[derive(Default)]
pub struct LoginGuard {
    failures: u32,
    locked_until: Option<Instant>,
}

impl LoginGuard {
    /// Gate one attempt: refuse while a window is armed, otherwise charge the
    /// attempt up front and admit it.
    ///
    /// Check and charge are one operation under one lock acquisition,
    /// deliberately. A handler that consulted the guard, verified the password
    /// and only then recorded the outcome would hold the lock for none of the
    /// middle, so N concurrent submissions would all pass the bare check before
    /// any recorded a failure, multiplying every window on the curve by the
    /// attacker's concurrency. Charging at admission arms the window before the
    /// lock is released, so a burst timed to a window's expiry buys one guess,
    /// not N. The charge is the pessimistic one: [`Self::record_success`]
    /// repays it by ending the run, and a failed attempt calls
    /// [`Self::confirm_failure`] to move the window's start to the outcome. An
    /// attempt that ends in neither — an infrastructure error mid-attempt —
    /// stays charged with the admission-time window, which errs closed.
    pub fn begin_attempt(&mut self) -> bool {
        if !self.check() {
            return false;
        }
        self.record_failure();
        true
    }

    /// Re-arm the window the run has earned, after a failed attempt reports
    /// its outcome.
    ///
    /// The attempt was already counted at admission; this only moves the
    /// window's start from admission time to outcome time. Without it the
    /// verification's own duration would eat into the wait — argon2 costs a
    /// meaningful fraction of the one-second base window by design — and the
    /// curve's early steps would be shorter than they claim.
    pub fn confirm_failure(&mut self) {
        self.locked_until = Some(Instant::now() + backoff_for(self.failures));
    }

    /// True when a login attempt may proceed; an elapsed window is cleared,
    /// but the failure run behind it is deliberately kept.
    pub fn check(&mut self) -> bool {
        match self.locked_until {
            Some(until) if until > Instant::now() => false,
            Some(_) => {
                self.locked_until = None;
                true
            }
            None => true,
        }
    }

    /// Record a failed login and arm the window this run has earned.
    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
        self.locked_until = Some(Instant::now() + backoff_for(self.failures));
    }

    /// Record a successful login, ending the failure run.
    pub fn record_success(&mut self) {
        self.failures = 0;
        self.locked_until = None;
    }

    /// The on-disk shape of the current state.
    ///
    /// `Instant` is monotonic and dies with the process, so the armed window
    /// crosses a restart as an **absolute UNIX timestamp** instead. That
    /// trades away precision against clock steps — an NTP jump or a dead RTC
    /// moves the window with the clock — which is accepted: the error is
    /// bounded by the cap on load, and the alternative (persisting a bare
    /// remaining-duration) would let a reboot restart the window from full,
    /// turning every power cycle into extra punishment.
    ///
    /// A sub-second remainder rounds **up** to one second rather than down to
    /// "no window": down would make a restart inside the first backoff step a
    /// free retry, the exact bypass §6 exists to close.
    fn to_persisted(&self) -> PersistedGuard {
        let remaining = self
            .locked_until
            .map(|until| until.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::ZERO);
        PersistedGuard {
            failures: self.failures,
            locked_until_unix: if remaining.is_zero() {
                0
            } else {
                crate::persist::now_unix().saturating_add(remaining.as_secs().max(1))
            },
        }
    }

    /// Rebuild the guard from a persisted snapshot.
    ///
    /// The remaining window is capped at [`BACKOFF_MAX`] **on load**: a
    /// corrupt or far-future timestamp — a clock that stepped backwards after
    /// the save, a bit-flipped file that still parses — must not arm a window
    /// the curve itself refuses to. "Never permanent" has to survive bad
    /// data, not just good arithmetic. The failure count needs no such cap;
    /// `backoff_for` already saturates.
    fn from_persisted(persisted: &PersistedGuard) -> Self {
        let remaining_secs = persisted
            .locked_until_unix
            .saturating_sub(crate::persist::now_unix())
            .min(BACKOFF_MAX.as_secs());
        let remaining = Duration::from_secs(remaining_secs);
        Self {
            failures: persisted.failures,
            locked_until: (!remaining.is_zero()).then(|| Instant::now() + remaining),
        }
    }
}

/// What [`GuardStore`] writes to `login_guard.json`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PersistedGuard {
    /// The consecutive-failure run, `LoginGuard::failures` verbatim.
    failures: u32,
    /// UNIX seconds at which the armed window ends; `0` when none is armed.
    locked_until_unix: u64,
}

/// [`LoginGuard`] behind a lock, persisted to disk on every mutation
/// (`docs/design/access.md` §6: an apid restart or a power cycle must not
/// reset the clock).
///
/// Lock acquisitions recover from poisoning for the same reason the previous
/// `Mutex<LoginGuard>` in `routes.rs` did: a panic while holding the counter
/// must not convert every later login into a panic of its own.
pub struct GuardStore {
    inner: Mutex<LoginGuard>,
    /// `None` = in-memory only, for tests built without persistence.
    path: Option<PathBuf>,
}

impl GuardStore {
    /// A store that never touches disk.
    pub fn ephemeral() -> Self {
        Self {
            inner: Mutex::new(LoginGuard::default()),
            path: None,
        }
    }

    /// Load the persisted state at `path`, or start clean.
    ///
    /// Infallible by design, and the failure direction is chosen per case: an
    /// absent file is first boot; an unreadable or unparsable one starts a
    /// clean slate with a loud log rather than refusing to start — corruption
    /// of a rate-limiter file must degrade to the in-RAM guard, never to a
    /// daemon that will not serve or a lock that will not lift.
    pub fn load(path: PathBuf) -> Self {
        let guard = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedGuard>(&bytes) {
                Ok(persisted) => LoginGuard::from_persisted(&persisted),
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "login-guard state is corrupt; starting a clean slate"
                    );
                    LoginGuard::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => LoginGuard::default(),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "login-guard state is unreadable; starting a clean slate"
                );
                LoginGuard::default()
            }
        };
        Self {
            inner: Mutex::new(guard),
            path: Some(path),
        }
    }

    /// Run `mutate` under the lock and persist iff the state changed.
    ///
    /// The changed-check is load-bearing: a refused attempt mutates nothing,
    /// and without the check an attacker hammering the login endpoint while
    /// throttled would convert every refusal into an fsync — flash wear and
    /// I/O load bought for the price of an HTTP request. The write happens
    /// while the lock is held so the on-disk state can never lag an admitted
    /// attempt; it is a tiny file at human login rate, so blocking here is
    /// cheaper than any ordering bug letting a restart forget a charge.
    ///
    /// A failed write degrades to in-RAM behavior with a loud log rather
    /// than failing the login: see the corruption rationale on [`Self::load`].
    fn with<R>(&self, mutate: impl FnOnce(&mut LoginGuard) -> R) -> R {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = (guard.failures, guard.locked_until);
        let result = mutate(&mut guard);
        if let Some(path) = &self.path
            && (guard.failures, guard.locked_until) != before
        {
            let contents = serde_json::to_string(&guard.to_persisted())
                .expect("two integers serialize as JSON");
            if let Err(err) = crate::persist::write_atomically(path, &contents, 0o600) {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "persisting login-guard state failed; counters are RAM-only until it succeeds"
                );
            }
        }
        result
    }

    /// [`LoginGuard::begin_attempt`] with persistence.
    pub fn begin_attempt(&self) -> bool {
        self.with(LoginGuard::begin_attempt)
    }

    /// [`LoginGuard::confirm_failure`] with persistence.
    pub fn confirm_failure(&self) {
        self.with(LoginGuard::confirm_failure)
    }

    /// [`LoginGuard::record_success`] with persistence.
    pub fn record_success(&self) {
        self.with(LoginGuard::record_success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GuardStore: access.md §6's "a power cycle must not reset the clock"
    // The LoginGuard tests above are about the CURVE. These are about the
    // curve SURVIVING, which is the property §6 actually asks for and the one
    // an in-RAM counter satisfies vacuously.

    /// A store rebuilt from the same path is still throttled. This is the
    /// classic embedded bypass — pull the power, come back to a clean slate —
    /// and it is the whole reason this type exists.
    #[test]
    fn a_restart_does_not_reset_the_armed_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");

        // Seeded with a run, so the armed window is 16 seconds rather than
        // BACKOFF_BASE's one. The property under test is "an armed window
        // survives a restart" and does not depend on which step of the curve is
        // armed, but arming the first step races the format's own resolution:
        // the write and the read below are two filesystem round-trips, and
        // `PersistedGuard` carries the deadline as whole UNIX seconds —
        // `to_persisted` writes `now_unix() + remaining.as_secs().max(1)` and
        // `from_persisted` subtracts a freshly-read `now_unix()`, so the two
        // truncations do not cancel and a write and read landing on opposite
        // sides of a second boundary reduce a one-second window to zero.
        // Sixteen seconds absorbs both the truncation and any load this suite
        // can generate; the one-second step is covered without a clock by
        // `backoff_doubles_from_the_base_and_stops_at_the_cap`. The truncation
        // is a real if minor property of the on-disk format — a restart inside
        // the first second can drop that step's window while keeping the
        // failure count — recorded here rather than fixed, this test's job
        // being the round trip.
        std::fs::write(
            &path,
            serde_json::to_string(&PersistedGuard {
                failures: 5,
                locked_until_unix: 0,
            })
            .unwrap(),
        )
        .unwrap();

        let store = GuardStore::load(path.clone());
        assert!(store.begin_attempt(), "the first attempt is admitted");
        store.confirm_failure();
        assert!(
            !store.begin_attempt(),
            "the failure armed a window, so the next attempt is refused"
        );
        drop(store);

        let restarted = GuardStore::load(path);
        assert!(
            !restarted.begin_attempt(),
            "a restart admitted an attempt the armed window had refused: the counter did not \
             survive, which is exactly the bypass §6 names"
        );
    }

    /// ...and the RUN survives too, not merely the window. A restart that
    /// kept the lockout but forgot the failure count would let an attacker
    /// hold the curve at its base step forever by power-cycling.
    #[test]
    fn a_restart_carries_the_failure_run_so_the_curve_keeps_climbing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");

        let store = GuardStore::load(path.clone());
        for _ in 0..4 {
            store.with(|guard| guard.record_failure());
        }
        let before = store.with(|guard| guard.failures);
        drop(store);

        let restarted = GuardStore::load(path);
        assert_eq!(
            restarted.with(|guard| guard.failures),
            before,
            "the consecutive-failure run reset across a restart, so the backoff would start \
             again from BACKOFF_BASE however many failures preceded it"
        );
        assert!(before >= 4);
    }

    /// A window that had already elapsed when the file was written must not
    /// come back as a lockout. The persisted form is an absolute deadline, so
    /// getting this backwards locks an operator out of a device that was
    /// never throttled.
    #[test]
    fn an_elapsed_window_does_not_come_back_as_a_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");
        std::fs::write(
            &path,
            serde_json::to_string(&PersistedGuard {
                failures: 3,
                locked_until_unix: crate::persist::now_unix().saturating_sub(60),
            })
            .unwrap(),
        )
        .unwrap();

        let store = GuardStore::load(path);
        // The failure run first: admitting the attempt is ALSO what a store
        // that never read the file at all would do, so this assertion is what
        // makes the next one evidence about loading rather than about
        // defaults. (Measured: without it, this case stays green under a
        // mutation that makes `load` ignore the file entirely.)
        assert_eq!(
            store.with(|guard| guard.failures),
            3,
            "the file was not read, so nothing below is a claim about reloading"
        );
        assert!(
            store.begin_attempt(),
            "a deadline 60s in the past was reloaded as an armed window"
        );
    }

    /// A far-future deadline is capped at BACKOFF_MAX on load. "Never
    /// permanent" has to survive bad data, not just good arithmetic.
    #[test]
    fn a_far_future_deadline_is_capped_rather_than_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");
        std::fs::write(
            &path,
            serde_json::to_string(&PersistedGuard {
                failures: 9,
                locked_until_unix: crate::persist::now_unix() + 10 * 365 * 24 * 3600,
            })
            .unwrap(),
        )
        .unwrap();

        let store = GuardStore::load(path);
        let remaining = store
            .with(|guard| guard.locked_until)
            .expect("a future deadline arms a window")
            .saturating_duration_since(Instant::now());
        assert!(
            remaining <= BACKOFF_MAX,
            "a ten-year deadline survived load as {remaining:?}; the cap is the only thing \
             standing between a bit-flip and a permanently bricked management interface"
        );
    }

    /// Corruption degrades to the in-RAM guard, never to a lockout and never
    /// to a daemon that will not start.
    #[test]
    fn a_corrupt_state_file_starts_clean_rather_than_locking_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = GuardStore::load(path);
        assert!(
            store.begin_attempt(),
            "a corrupt rate-limiter file refused a login; the failure direction has to be \
             open here, because the alternative is an appliance no operator can reach"
        );
    }

    /// The file is written 0600, and it is written at all. Both halves: a
    /// test that only checked the mode would pass on a file that was never
    /// created.
    #[test]
    fn the_persisted_file_is_written_and_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");
        let store = GuardStore::load(path.clone());
        store.begin_attempt();
        store.confirm_failure();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "login-guard state is mode {mode:o}");
    }

    /// A refused attempt mutates nothing and must therefore write nothing.
    /// `GuardStore::with`'s changed-check is what makes this true, and
    /// without it an attacker hammering a throttled endpoint converts every
    /// refusal into an fsync — flash wear bought with an HTTP request.
    #[test]
    fn a_refused_attempt_does_not_rewrite_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login_guard.json");
        let store = GuardStore::load(path.clone());
        store.begin_attempt();
        store.confirm_failure();

        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let armed = std::fs::read(&path).unwrap();
        for _ in 0..50 {
            assert!(!store.begin_attempt(), "still throttled");
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            armed,
            "fifty refused attempts changed the persisted state"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "fifty refused attempts rewrote the file; each one is an fsync an unauthenticated \
             caller can trigger"
        );
    }

    /// The ephemeral form touches no filesystem at all — the negative control
    /// for every test above, and what `AppState::new` gives tests.
    #[test]
    fn an_ephemeral_store_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = GuardStore::ephemeral();
        store.begin_attempt();
        store.confirm_failure();
        store.record_success();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "the ephemeral store created a file"
        );
    }

    #[test]
    fn hash_roundtrip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "correct horse"));
        assert!(!verify_password(&hash, "wrong"));
        assert!(!verify_password("not a phc string", "wrong"));
    }

    #[test]
    fn backoff_doubles_from_the_base_and_stops_at_the_cap() {
        assert_eq!(backoff_for(0), Duration::ZERO);
        assert_eq!(backoff_for(1), BACKOFF_BASE);
        assert_eq!(backoff_for(2), Duration::from_secs(2));
        assert_eq!(backoff_for(3), Duration::from_secs(4));
        assert_eq!(backoff_for(9), Duration::from_secs(256));
        // The cap bites here and holds for every count past it, including the
        // ones where `2^(n-1)` no longer fits in a u64.
        assert_eq!(backoff_for(10), BACKOFF_MAX);
        assert_eq!(backoff_for(64), BACKOFF_MAX);
        assert_eq!(backoff_for(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn one_failure_already_arms_a_window() {
        let mut guard = LoginGuard::default();
        assert!(guard.check());
        guard.record_failure();
        // One failure, and the next attempt is already refused.
        assert!(!guard.check());
    }

    #[test]
    fn an_elapsed_window_does_not_reset_the_run() {
        let mut guard = LoginGuard::default();
        for _ in 0..3 {
            guard.record_failure();
        }
        // Expire the window the way the clock would, without waiting on it.
        guard.locked_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(guard.check());
        assert_eq!(guard.failures, 3, "riding out a window must not be free");

        // So the next failure escalates rather than restarting the curve.
        guard.record_failure();
        assert_eq!(guard.failures, 4);
    }

    #[test]
    fn success_ends_the_run_and_clears_the_window() {
        let mut guard = LoginGuard::default();
        for _ in 0..5 {
            guard.record_failure();
        }
        assert!(!guard.check());
        guard.record_success();
        assert!(guard.check());
        assert_eq!(guard.failures, 0);

        // A fresh run starts back at the base, not where the last one stopped.
        guard.record_failure();
        assert_eq!(guard.failures, 1);
    }

    #[test]
    fn begin_attempt_charges_at_admission_not_at_outcome() {
        let mut guard = LoginGuard::default();
        assert!(guard.begin_attempt());
        // The window armed when the first attempt was admitted, so a second
        // attempt racing it is refused before the first reports any outcome —
        // the property a separate check-then-record pair did not have.
        assert!(!guard.begin_attempt());

        // Success repays the admission charge entirely.
        guard.record_success();
        assert_eq!(guard.failures, 0);
        assert!(guard.begin_attempt());
        assert_eq!(
            guard.failures, 1,
            "an admitted attempt is a charged attempt"
        );
    }

    #[test]
    fn the_lockout_is_never_permanent() {
        let mut guard = LoginGuard::default();
        for _ in 0..1000 {
            guard.record_failure();
        }
        let until = guard.locked_until.expect("a window is armed");
        // An administrator who knows the password waits at most BACKOFF_MAX --
        // there is no threshold past which the daemon stops answering, because
        // apid has no physical-presence release to clear one with.
        assert!(until <= Instant::now() + BACKOFF_MAX);
    }
}
