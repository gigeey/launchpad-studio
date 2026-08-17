//! Durable single-writer lease persistence.
//!
//! [`ChannelLeaseStore`] gives each `(agent_id, binding_id)` channel binding
//! a small on-disk claim: [`ChannelBridge`](../../ao_engine/telegram/struct.ChannelBridge.html)
//! (the shared inbound supervisor for Telegram, Discord, and email) checks
//! and renews it before running a binding, so a second process pointed at
//! the same data dir — the two-worktree scenario this store exists to
//! close — is refused rather than double-connecting the same bot.
//!
//! `try_claim` is both the initial claim *and* the heartbeat: called
//! repeatedly by the same `owner_id` it just keeps extending `expires_at`;
//! called by a different `owner_id` it only succeeds once the previous
//! claim's TTL has lapsed. That's what makes a hard crash recoverable
//! instead of wedging the binding — no separate "is the old owner still
//! alive" check is needed, the lease just ages out.
//!
//! Not a secret, so unlike `ChannelSecretStore` this never touches the OS
//! keychain — a plain JSON file per binding under the data root, same as
//! `ChannelCursorStore`.
//!
//! **Guarantee and its limits.** `try_claim` gives exactly one caller
//! `Ok(true)` for a given `(agent_id, binding_id)` at a time, including when
//! N processes race to reclaim the same just-expired lease — see the lock
//! guard below. That guarantee is **filesystem-local**: it serializes only
//! processes that share the directory the lease file lives in (the same
//! machine, or a network filesystem multiple machines mount to the same
//! path). It is *not* a distributed lock — two backend nodes each pointed
//! at their own local disk will each happily create and hold "the" lease
//! for the same binding. Fine for the desktop/two-worktree case this store
//! exists to close; a real multi-node deployment needs a lock service, not
//! this store.
//!
//! **Lifecycle observability.** Every lease state change [`Self::try_claim`]
//! and [`Self::release`] make logs a structured event with the stable
//! `"channel lease: "` message prefix — `grep "channel lease:"` finds every
//! acquire, renew, renew-failure, and release across a process's lifetime,
//! which is otherwise only inferable by diffing lease files on disk by
//! hand. `acquired` additionally carries the previous owner (if any) and
//! `dead_air_secs` — how long the lease sat expired before this claim, the
//! machine-readable failover-latency measurement. `renewed` (the ordinary
//! ~5s heartbeat) logs at `DEBUG`; every other event logs at `INFO`. A
//! binding losing its lease to another owner (as opposed to this store
//! successfully renewing or releasing one) is logged by the caller instead
//! — see `ChannelBridge::reconcile`'s own `"channel lease: lost"` event —
//! since only the caller knows whether it was already running the binding
//! it's now being refused.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use tracing::{debug, info};

use ao_protocol::channel_lease::ChannelLease;
use ao_protocol::error::AoError;

use crate::paths::DataRoot;

/// How old a reclaim lock file may get before it's treated as abandoned by
/// a holder that crashed mid-critical-section, rather than a live
/// contender still doing its read-check-write. Deliberately generous next
/// to how briefly the lock is actually held (one small read + one small
/// write) — this only fires on an actual crash, not ordinary contention.
const RECLAIM_LOCK_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Margin [`RECLAIM_LOCK_WAIT_BUDGET`] adds on top of
/// [`RECLAIM_LOCK_STALE_AFTER`] — see that constant for why the margin must
/// be strictly positive.
const RECLAIM_LOCK_WAIT_MARGIN: std::time::Duration = std::time::Duration::from_secs(2);

/// Total time `try_claim` will spend retrying against a live (non-stale)
/// reclaim lock before giving up. Contention only ever holds the lock for a
/// read+write of a few bytes, so this bounds a worst case that should never
/// actually happen; it exists to fail loudly instead of hanging forever if
/// it does.
///
/// Deliberately derived as [`RECLAIM_LOCK_STALE_AFTER`] plus a margin,
/// rather than an independent constant: this budget is what lets a single
/// `acquire_reclaim_lock` call self-heal from an abandoned lock entirely on
/// its own. If it were shorter than (or equal to) the stale threshold, a
/// call could never itself watch a lock cross the staleness line — it
/// would give up on a bare timeout first, and only recover by accident of
/// some *external* caller retrying later (e.g. the reconcile loop's own 5s
/// tick). A one-shot caller with no such retry loop would then wedge
/// forever on an abandoned lock instead of breaking it. See the
/// const-invariant test `reclaim_lock_wait_budget_exceeds_stale_after`.
const RECLAIM_LOCK_WAIT_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(RECLAIM_LOCK_STALE_AFTER.as_secs() + RECLAIM_LOCK_WAIT_MARGIN.as_secs());

const RECLAIM_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(2);

/// On-disk store for one [`ChannelLease`] per `(agent_id, binding_id)`. See
/// the module doc for the atomicity guarantee [`Self::try_claim`] makes and
/// its filesystem-local limits.
pub struct ChannelLeaseStore {
    data_root: DataRoot,
}

/// RAII guard for the reclaim lock file `try_claim` holds across its
/// read-check-write of a contested lease. Removes the lock file on drop —
/// normal return, an early `Ok(false)`, or a propagated `Err` via `?` all
/// unwind through this guard, and `Drop` always runs. Deliberately uses
/// blocking `std::fs::remove_file` rather than `tokio::fs`: `Drop` can't be
/// `async`, and this unlinks a single small file this process just created,
/// so the blocking cost is negligible — including the cancellation-safety
/// case where the enclosing future is dropped mid-await while the lock is
/// held, which runs this `Drop` synchronously with no runtime needed.
struct ReclaimLockGuard {
    lock_path: PathBuf,
}

impl Drop for ReclaimLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

impl ChannelLeaseStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Reads the persisted lease for `(agent_id, binding_id)`, if any. Not
    /// used by `reconcile`'s hot path (which goes straight through
    /// [`Self::try_claim`]) — exposed for callers that just want to observe
    /// current ownership (e.g. a future connection-state surface).
    pub async fn get(
        &self,
        agent_id: &str,
        binding_id: &str,
    ) -> Result<Option<ChannelLease>, AoError> {
        Self::read(&self.data_root.channel_lease_path(agent_id, binding_id)).await
    }

    /// Attempts to claim (or, for the current holder, heartbeat/renew) the
    /// lease on `(agent_id, binding_id)` for `owner_id` as of `now`.
    ///
    /// Returns `Ok(true)` — the caller now holds the lease until
    /// `now + ttl` — when:
    /// - nothing is currently persisted for this binding,
    /// - the persisted lease already belongs to `owner_id` (the heartbeat
    ///   path), or
    /// - the persisted lease belongs to someone else but has expired as of
    ///   `now` (the crash-recovery path).
    ///
    /// Returns `Ok(false)`, leaving the persisted lease untouched, when a
    /// different `owner_id`'s lease is still live — the refusal path a
    /// second claimant hits.
    ///
    /// **Atomicity.** Among any number of callers racing to claim the same
    /// `(agent_id, binding_id)` — whether nothing is persisted yet, or an
    /// existing lease has just expired — exactly one gets `Ok(true)`. The
    /// first-ever-claim case is atomic via `create_new` on the lease file
    /// itself. The reclaim/renew case additionally serializes its
    /// read-check-write through a short-lived `.lock` sibling file (also
    /// via `create_new`), so two processes that both observe the same
    /// expired lease can't both write a winning lease record — see
    /// [`ReclaimLockGuard`]. As stated on the module doc, this is a
    /// filesystem-local guarantee, not a distributed one.
    ///
    /// **Heartbeat fast path.** The ordinary renewal case above — the
    /// current owner extending its own still-live lease, which is what
    /// every ~5s reconcile tick does for as long as a binding keeps running
    /// — skips the reclaim lock entirely and writes the renewal directly.
    /// It never needs the lock: nothing else can legitimately be contending
    /// for a lease this call itself can see is both ours and unexpired.
    pub async fn try_claim(
        &self,
        agent_id: &str,
        binding_id: &str,
        owner_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<bool, AoError> {
        let path = self.data_root.channel_lease_path(agent_id, binding_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Exclusive create wins the lease outright when nothing has ever
        // been persisted for this binding. This is the atomic path that
        // protects the case two brand-new processes race to claim the same
        // binding for the first time — `create_new` fails with
        // `AlreadyExists` for exactly one of them regardless of scheduling.
        let fresh = ChannelLease { owner_id: owner_id.to_string(), claimed_at: now, expires_at: now + ttl };
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                let json =
                    serde_json::to_vec_pretty(&fresh).map_err(|e| AoError::Json(e.to_string()))?;
                file.write_all(&json).await?;
                file.flush().await?;
                info!(
                    agent_id, binding_id, owner_id,
                    previous_owner_id = ?Option::<String>::None,
                    dead_air_secs = ?Option::<i64>::None,
                    "channel lease: acquired"
                );
                return Ok(true);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }

        // Heartbeat fast path. Once a lease file exists — true for every
        // running binding after its first tick — `create_new` above always
        // misses, so without this every ~5s renewal would fall through to
        // the fully contested path below: create a lock file, write it,
        // read the lease, tmp+rename it, remove the lock — six-plus
        // syscalls just to renew a lease this process already legitimately
        // owns. No compare-and-swap is needed to renew a lease you already
        // hold, so read it unlocked and, if it's still ours and still live,
        // extend it directly.
        //
        // Safe ONLY because a competing process can only ever be attempting
        // a *reclaim*, which by definition requires the persisted lease to
        // read as expired. So the moment any of that isn't true — expired,
        // owned by someone else, or missing/unparseable — this must fall
        // through to the full locked path below instead of guessing.
        if let Ok(Some(current)) = Self::read(&path).await {
            if current.owner_id == owner_id && !current.is_expired(now) {
                let renewed = ChannelLease {
                    owner_id: owner_id.to_string(),
                    claimed_at: current.claimed_at,
                    expires_at: now + ttl,
                };
                match Self::write(&path, &renewed).await {
                    Ok(()) => {
                        debug!(agent_id, binding_id, owner_id, "channel lease: renewed");
                        return Ok(true);
                    }
                    Err(e) => {
                        info!(agent_id, binding_id, owner_id, error = %e, "channel lease: renewed-failed");
                        return Err(e.into());
                    }
                }
            }
        }

        // Something is already persisted for this binding — only a renewal
        // by the current owner or a reclaim of an expired lease may
        // overwrite it. Both are a read-check-write, so both go under the
        // reclaim lock: without it, two processes that both read the same
        // expired lease would both see `can_claim == true` and both write,
        // and the lease would silently gain two simultaneous "sole" owners.
        // The lock's own acquisition is atomic (`create_new`, same
        // primitive as the fresh-claim path above), so only one caller at a
        // time ever gets past this point for a given lease file.
        let lock_path = Self::reclaim_lock_path(&path);
        let _lock = Self::acquire_reclaim_lock(&lock_path).await?;

        let current = Self::read(&path).await?;
        let can_claim = match &current {
            None => true,
            Some(lease) => lease.owner_id == owner_id || lease.is_expired(now),
        };
        if !can_claim {
            return Ok(false);
        }
        let claimed_at = match &current {
            Some(lease) if lease.owner_id == owner_id => lease.claimed_at,
            _ => now,
        };
        let lease = ChannelLease { owner_id: owner_id.to_string(), claimed_at, expires_at: now + ttl };
        Self::write(&path, &lease).await?;
        // Reaching this point (rather than the heartbeat fast path above)
        // always means a *new* claim was just established — either nothing
        // was persisted before, or the persisted lease had expired — so this
        // is always an "acquired" event, never a "renewed" one. `now`, not
        // `claimed_at`, measures the dead air: for a same-owner reclaim of
        // its own lapsed lease, `claimed_at` is deliberately preserved as
        // the *original* claim time above, which would otherwise understate
        // (or even negate) how long the lease actually sat expired.
        let previous_owner_id = current.as_ref().map(|lease| lease.owner_id.clone());
        let dead_air_secs = current.as_ref().map(|lease| (now - lease.expires_at).num_seconds());
        info!(
            agent_id, binding_id, owner_id,
            previous_owner_id = ?previous_owner_id,
            dead_air_secs = ?dead_air_secs,
            "channel lease: acquired"
        );
        Ok(true)
    }

    fn reclaim_lock_path(lease_path: &Path) -> PathBuf {
        lease_path.with_file_name(format!(
            "{}.lock",
            lease_path.file_name().and_then(|f| f.to_str()).unwrap_or("channel_lease")
        ))
    }

    /// Acquires the reclaim lock at `lock_path`, blocking (via short async
    /// retries, not an OS-level block) until it does, an abandoned lock is
    /// broken, or [`RECLAIM_LOCK_WAIT_BUDGET`] is exhausted.
    ///
    /// Each attempt is a `create_new` — the same atomic primitive as the
    /// fresh-lease path — so exactly one racing caller wins it per retry
    /// round. A caller that loses checks the winner's lock *age*: past
    /// [`RECLAIM_LOCK_STALE_AFTER`] old, the lock's owner is presumed
    /// crashed while holding it, so this removes it and retries rather than
    /// waiting out the full budget — that's what keeps a crash from
    /// wedging every future reclaim of this binding forever, mirroring the
    /// TTL that already does this job for the lease itself.
    async fn acquire_reclaim_lock(lock_path: &Path) -> Result<ReclaimLockGuard, AoError> {
        let deadline = tokio::time::Instant::now() + RECLAIM_LOCK_WAIT_BUDGET;
        loop {
            match tokio::fs::OpenOptions::new().write(true).create_new(true).open(lock_path).await
            {
                Ok(mut file) => {
                    // Content is diagnostic only (helps a human reading the
                    // data dir mid-incident); staleness below is judged
                    // from the file's own filesystem metadata, not this
                    // content, so a slow writer here can never make a
                    // reader mistake a live lock for an abandoned one.
                    use tokio::io::AsyncWriteExt;
                    let stamp = Utc::now().to_rfc3339();
                    let _ = file.write_all(stamp.as_bytes()).await;
                    let _ = file.flush().await;
                    return Ok(ReclaimLockGuard { lock_path: lock_path.to_path_buf() });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match Self::reclaim_lock_age(lock_path).await {
                        Some(age) if age > RECLAIM_LOCK_STALE_AFTER => {
                            // Best-effort: if another caller is
                            // simultaneously breaking (or just re-created)
                            // the same stale lock, the next loop
                            // iteration's `create_new` arbitrates who
                            // actually wins it, so a failed or redundant
                            // removal here is harmless.
                            let _ = tokio::fs::remove_file(lock_path).await;
                            continue;
                        }
                        Some(_) => {
                            // Live lock, held by a real contender — wait it
                            // out below.
                        }
                        None => {
                            // The lock vanished between our failed
                            // `create_new` and this metadata read — its
                            // holder released it normally. Retry
                            // immediately rather than backing off.
                            continue;
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(AoError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "timed out waiting for channel lease reclaim lock at {}",
                                lock_path.display()
                            ),
                        )));
                    }
                    tokio::time::sleep(RECLAIM_LOCK_RETRY_DELAY).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// How long ago `lock_path` was created (by mtime), or `None` if it no
    /// longer exists. Deliberately reads filesystem metadata rather than
    /// parsing the lock file's content: the OS sets a file's mtime
    /// atomically at `create_new` time, before any writer gets to fill it
    /// in, so this can't be fooled by racing a lock's brief
    /// created-but-not-yet-written window into misjudging a live lock as
    /// stale — a content-based check with a "missing/unparseable ⇒ stale"
    /// fallback can, and did (caught by
    /// `try_claim_reclaim_is_atomic_under_real_thread_contention` during
    /// development).
    async fn reclaim_lock_age(lock_path: &Path) -> Option<std::time::Duration> {
        let metadata = tokio::fs::metadata(lock_path).await.ok()?;
        let modified = metadata.modified().ok()?;
        std::time::SystemTime::now().duration_since(modified).ok()
    }

    /// Releases the lease on `(agent_id, binding_id)` iff it is currently
    /// held by `owner_id` — a no-op otherwise, so a process that has
    /// already lost (or never held) a lease can never delete someone
    /// else's active claim. Called when `reconcile` intentionally stops a
    /// binding it still holds (disabled, reconfigured, or shutting down),
    /// so the lease is immediately claimable rather than making the next
    /// owner wait out the TTL.
    pub async fn release(&self, agent_id: &str, binding_id: &str, owner_id: &str) -> Result<(), AoError> {
        let path = self.data_root.channel_lease_path(agent_id, binding_id);
        match Self::read(&path).await? {
            Some(lease) if lease.owner_id == owner_id => match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    info!(agent_id, binding_id, owner_id, "channel lease: released");
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
            _ => Ok(()),
        }
    }

    async fn read(path: &Path) -> Result<Option<ChannelLease>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path).await?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let lease: ChannelLease =
            serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(lease))
    }

    async fn write(path: &Path, lease: &ChannelLease) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(lease).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("channel_lease"),
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store_at(dir: &Path) -> ChannelLeaseStore {
        ChannelLeaseStore::new(DataRoot::new(dir))
    }

    #[tokio::test]
    async fn get_returns_none_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), None);
    }

    #[tokio::test]
    async fn try_claim_succeeds_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        let claimed = store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap();
        assert!(claimed);

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-1");
        assert_eq!(lease.claimed_at, now);
        assert_eq!(lease.expires_at, now + Duration::seconds(15));
    }

    #[tokio::test]
    async fn try_claim_refuses_a_second_owner_while_the_lease_is_live() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        assert!(store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap());

        // A different owner, moments later, well inside the TTL — refused.
        let refused = store
            .try_claim(
                "agent-a",
                "telegram",
                "owner-2",
                Duration::seconds(15),
                now + Duration::seconds(1),
            )
            .await
            .unwrap();
        assert!(!refused, "a second owner must not steal a live lease");

        // The original owner's claim is untouched.
        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-1");
    }

    /// Regression test for the reclaim-path race: N processes that all read
    /// the same expired lease before any of them writes must not all
    /// observe `can_claim == true`. Uses real `std::thread`s (each driving
    /// its own single-threaded Tokio runtime), not concurrent futures on
    /// one runtime — the bug this guards against is a filesystem race, and
    /// cooperatively-scheduled futures on one thread never actually
    /// interleave their `create_new` syscalls the way independent OS
    /// threads do. Confirmed (manually, then reverted) that this fails
    /// reliably — every iteration sees `successes > 1` — against the
    /// pre-fix `try_claim` that reads, checks, and writes with no lock.
    #[test]
    fn try_claim_reclaim_is_atomic_under_real_thread_contention() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        const THREADS: usize = 8;
        const ITERATIONS: usize = 25;

        for iteration in 0..ITERATIONS {
            let tmp = tempdir().unwrap();
            let now = Utc::now();

            // Seed a lease that's already expired by the time the racing
            // threads below call `try_claim`, so every one of them takes
            // the contested reclaim path rather than the uncontested
            // fresh-claim path.
            let seed_rt = tokio::runtime::Runtime::new().unwrap();
            let seeded = seed_rt.block_on(store_at(tmp.path()).try_claim(
                "agent-a",
                "telegram",
                "original-owner",
                Duration::seconds(1),
                now - Duration::seconds(5),
            ));
            assert!(seeded.unwrap());

            let successes = Arc::new(AtomicUsize::new(0));
            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let dir = tmp.path().to_path_buf();
                    let successes = Arc::clone(&successes);
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        let store = store_at(&dir);
                        let claimed = rt
                            .block_on(store.try_claim(
                                "agent-a",
                                "telegram",
                                &format!("owner-{i}"),
                                Duration::seconds(15),
                                now,
                            ))
                            .unwrap();
                        if claimed {
                            successes.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }

            let won = successes.load(Ordering::SeqCst);
            assert_eq!(
                won, 1,
                "iteration {iteration}: exactly one of {THREADS} racing reclaims must win, got {won}"
            );
        }
    }

    /// A reclaim lock left behind by a holder that crashed mid
    /// critical-section (after creating the lock, before removing it) must
    /// not permanently wedge the binding — same no-permanent-deadlock
    /// requirement the lease TTL itself satisfies for a crashed lease
    /// owner.
    #[tokio::test]
    async fn try_claim_reclaims_when_a_previous_reclaim_lock_was_abandoned() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        // Original owner's lease, already expired as of `now`.
        assert!(store
            .try_claim(
                "agent-a",
                "telegram",
                "owner-1",
                Duration::seconds(1),
                now - Duration::seconds(10),
            )
            .await
            .unwrap());

        // Simulate a second process that crashed after acquiring the
        // reclaim lock but before releasing it: a lock file sitting next
        // to the (also expired) lease, backdated past
        // `RECLAIM_LOCK_STALE_AFTER` so it reads as abandoned rather than
        // live. Staleness is judged from filesystem mtime, not content
        // (see `reclaim_lock_age`), so the file's mtime — not its bytes —
        // is what has to be backdated here.
        let data_root = DataRoot::new(tmp.path());
        let lease_path = data_root.channel_lease_path("agent-a", "telegram");
        let lock_path = ChannelLeaseStore::reclaim_lock_path(&lease_path);
        tokio::fs::create_dir_all(lock_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&lock_path, "abandoned-lock").await.unwrap();
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap()
            .set_modified(backdated)
            .unwrap();

        let reclaimed = store
            .try_claim("agent-a", "telegram", "owner-2", Duration::seconds(15), now)
            .await
            .unwrap();
        assert!(reclaimed, "an abandoned reclaim lock must not block a legitimate reclaim");

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-2");

        // The winner must also clean up after itself — no lock file left
        // over for the next caller to have to break.
        assert!(!lock_path.exists(), "reclaim lock must be released once try_claim returns");
    }

    #[tokio::test]
    async fn try_claim_reclaims_an_expired_lease_for_a_new_owner() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        assert!(store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap());

        // Exactly at expiry (inclusive) and past it, a new owner may reclaim
        // — this is what makes a hard crash recoverable rather than wedging
        // the binding forever.
        let past_expiry = now + Duration::seconds(15);
        let reclaimed = store
            .try_claim("agent-a", "telegram", "owner-2", Duration::seconds(15), past_expiry)
            .await
            .unwrap();
        assert!(reclaimed, "an expired lease must be claimable by a new owner");

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-2");
        assert_eq!(lease.claimed_at, past_expiry);
    }

    #[tokio::test]
    async fn try_claim_heartbeat_extends_expiry_for_the_current_owner() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        assert!(store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap());

        // Same owner, heartbeating before expiry — succeeds and pushes
        // expires_at further out, while claimed_at (the original claim
        // time) is preserved.
        let heartbeat_at = now + Duration::seconds(10);
        let renewed = store
            .try_claim(
                "agent-a",
                "telegram",
                "owner-1",
                Duration::seconds(15),
                heartbeat_at,
            )
            .await
            .unwrap();
        assert!(renewed);

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-1");
        assert_eq!(lease.claimed_at, now, "heartbeat must not reset the original claim time");
        assert_eq!(lease.expires_at, heartbeat_at + Duration::seconds(15));

        // A rival still can't claim in between — the lease the heartbeat
        // just renewed is live again from `heartbeat_at`.
        let refused = store
            .try_claim(
                "agent-a",
                "telegram",
                "owner-2",
                Duration::seconds(15),
                heartbeat_at + Duration::seconds(1),
            )
            .await
            .unwrap();
        assert!(!refused);
    }

    /// The heartbeat fast path (renewing a live lease this call already
    /// sees as its own) must never touch the reclaim lock at all — not
    /// create it, not wait on it, not care whether it's held. Proven here
    /// by planting a *live* (freshly-timestamped, non-stale) lock file
    /// before the renewal: if the fast path were absent or misfired,
    /// `try_claim` would fall through to the locked path, find that lock
    /// live, and spend the whole `RECLAIM_LOCK_WAIT_BUDGET` retrying before
    /// giving up with an error. Instead this must return `Ok(true)`
    /// immediately, leaving the planted lock file completely untouched.
    #[tokio::test]
    async fn try_claim_heartbeat_fast_path_ignores_a_live_reclaim_lock() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        assert!(store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap());

        let data_root = DataRoot::new(tmp.path());
        let lease_path = data_root.channel_lease_path("agent-a", "telegram");
        let lock_path = ChannelLeaseStore::reclaim_lock_path(&lease_path);
        tokio::fs::write(&lock_path, "held-by-someone-else").await.unwrap();

        let heartbeat_at = now + Duration::seconds(5);
        let renewed = store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), heartbeat_at)
            .await
            .unwrap();
        assert!(renewed, "current-owner renewal of a live lease must not block on the reclaim lock");

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.expires_at, heartbeat_at + Duration::seconds(15));

        assert_eq!(
            tokio::fs::read_to_string(&lock_path).await.unwrap(),
            "held-by-someone-else",
            "the fast path must never create, inspect, or remove the reclaim lock"
        );
    }

    /// A different owner contending a still-live lease must still go
    /// through the locked path — the fast path only ever fires for the
    /// current owner. Proven by planting a *stale* (backdated) lock file
    /// first: only code that actually runs `acquire_reclaim_lock` would
    /// ever notice and clean it up, so if it's gone afterward, the locked
    /// path really executed (and the refusal below isn't a coincidence of
    /// some unrelated fast path).
    #[tokio::test]
    async fn try_claim_different_owner_on_a_live_lease_still_uses_the_locked_path() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        assert!(store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap());

        let data_root = DataRoot::new(tmp.path());
        let lease_path = data_root.channel_lease_path("agent-a", "telegram");
        let lock_path = ChannelLeaseStore::reclaim_lock_path(&lease_path);
        tokio::fs::write(&lock_path, "abandoned-lock").await.unwrap();
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new().write(true).open(&lock_path).unwrap().set_modified(backdated).unwrap();

        let refused = store
            .try_claim(
                "agent-a",
                "telegram",
                "owner-2",
                Duration::seconds(15),
                now + Duration::seconds(1),
            )
            .await
            .unwrap();
        assert!(!refused, "a second owner must not steal a live lease");

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-1", "the original owner's claim is untouched");
        assert!(
            !lock_path.exists(),
            "the locked path must have run (and cleaned up) to reach this refusal, proving no fast path fired"
        );
    }

    /// Same idea as above, but for the owner's *own already-expired* lease:
    /// the fast path's expiry check must gate on the lease, not just the
    /// owner id, or a stalled owner reclaiming its own lapsed lease would
    /// wrongly skip the lock that the reclaim guarantee depends on. Proven
    /// the same way — a planted stale lock must get cleaned up by the real
    /// locked path, not left behind by an accidental fast-path bypass.
    #[tokio::test]
    async fn try_claim_same_owner_on_an_expired_lease_still_uses_the_locked_path() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        assert!(store
            .try_claim(
                "agent-a",
                "telegram",
                "owner-1",
                Duration::seconds(1),
                now - Duration::seconds(10),
            )
            .await
            .unwrap());

        let data_root = DataRoot::new(tmp.path());
        let lease_path = data_root.channel_lease_path("agent-a", "telegram");
        let lock_path = ChannelLeaseStore::reclaim_lock_path(&lease_path);
        tokio::fs::write(&lock_path, "abandoned-lock").await.unwrap();
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new().write(true).open(&lock_path).unwrap().set_modified(backdated).unwrap();

        let reclaimed = store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap();
        assert!(reclaimed, "the owner of its own expired lease must still be able to reclaim it");

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-1");
        assert_eq!(
            lease.claimed_at,
            now - Duration::seconds(10),
            "the locked path preserves claimed_at whenever owner_id matches, expired or not"
        );
        assert!(
            !lock_path.exists(),
            "the locked path must have run (and cleaned up) to reach this reclaim, proving expiry gates the fast path"
        );
    }

    /// Const-level guard for the derivation `RECLAIM_LOCK_WAIT_BUDGET =
    /// RECLAIM_LOCK_STALE_AFTER + RECLAIM_LOCK_WAIT_MARGIN`: the budget must
    /// stay strictly greater than the stale threshold, or a single
    /// `acquire_reclaim_lock` call could never itself observe a lock cross
    /// the staleness line and would silently depend on an external retry
    /// loop (e.g. the reconcile loop's own tick) to self-heal.
    #[test]
    fn reclaim_lock_wait_budget_exceeds_stale_after() {
        assert!(
            RECLAIM_LOCK_WAIT_BUDGET > RECLAIM_LOCK_STALE_AFTER,
            "RECLAIM_LOCK_WAIT_BUDGET ({RECLAIM_LOCK_WAIT_BUDGET:?}) must exceed \
             RECLAIM_LOCK_STALE_AFTER ({RECLAIM_LOCK_STALE_AFTER:?}) so a single call can self-heal"
        );
    }

    #[tokio::test]
    async fn release_removes_a_lease_held_by_the_owner() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap();
        store.release("agent-a", "telegram", "owner-1").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), None);

        // Now claimable immediately by anyone, without waiting out a TTL.
        let claimed = store
            .try_claim("agent-a", "telegram", "owner-2", Duration::seconds(15), now)
            .await
            .unwrap();
        assert!(claimed);
    }

    #[tokio::test]
    async fn release_is_a_noop_when_the_caller_does_not_hold_the_lease() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap();

        // A non-owner's release must not touch the active lease.
        store.release("agent-a", "telegram", "owner-2").await.unwrap();

        let lease = store.get("agent-a", "telegram").await.unwrap().unwrap();
        assert_eq!(lease.owner_id, "owner-1");
    }

    #[tokio::test]
    async fn release_of_nonexistent_lease_is_a_noop() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        store.release("agent-a", "telegram", "owner-1").await.unwrap();
    }

    #[tokio::test]
    async fn different_bindings_on_the_same_agent_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap();
        store
            .try_claim("agent-a", "discord", "owner-2", Duration::seconds(15), now)
            .await
            .unwrap();

        assert_eq!(
            store.get("agent-a", "telegram").await.unwrap().unwrap().owner_id,
            "owner-1"
        );
        assert_eq!(
            store.get("agent-a", "discord").await.unwrap().unwrap().owner_id,
            "owner-2"
        );
    }

    #[tokio::test]
    async fn different_agents_with_the_same_binding_id_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store
            .try_claim("agent-a", "telegram", "owner-1", Duration::seconds(15), now)
            .await
            .unwrap();
        store
            .try_claim("agent-b", "telegram", "owner-2", Duration::seconds(15), now)
            .await
            .unwrap();

        assert_eq!(
            store.get("agent-a", "telegram").await.unwrap().unwrap().owner_id,
            "owner-1"
        );
        assert_eq!(
            store.get("agent-b", "telegram").await.unwrap().unwrap().owner_id,
            "owner-2"
        );
    }
}
