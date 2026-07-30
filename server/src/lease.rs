// SPDX-License-Identifier: MIT
//! Writer lease + epoch fencing — a stale writer is fenced even holding a live handle.
//!
//! # Why this exists
//!
//! The engine is a single-writer over a shared store directory. If a *second*
//! writer starts against the same directory while a *first* is still running —
//! a botched failover, a forgotten process, a split brain — the classic hazard
//! is that BOTH believe they may write, and their commits interleave into one
//! corrupted history. A lock alone does not close this: the first writer may
//! still be holding a live handle it acquired before it lost the lock.
//!
//! The fence is an **epoch** (a monotonic [`u64`] fencing token). Every writer
//! that takes the lease mints a strictly higher epoch and stamps it on every
//! commit it makes. Taking over is therefore *loud*: the incumbent, on its very
//! next commit, sees that the authority's current epoch has moved past the one
//! it holds and **refuses to commit** — it is fenced. A stale writer cannot
//! append to the durable log even though its in-process handle is perfectly
//! alive. (See the commit boundary in [`crate::engine::KanbanEngine::apply`].)
//!
//! # The seam
//!
//! [`WriterLease`] is the interface a deployment configures its lease authority
//! through — exactly as [`crate::storage::StorageBackend`] is the interface it
//! configures durability through. The shipped default, [`LocalWriterLease`], is
//! a single-host, pure-`std` implementation: an owner-metadata file plus a
//! persisted monotonic epoch, with crash recovery by owner-pid liveness. A
//! multi-host deployment supplies a different `WriterLease` (e.g. one backed by a
//! shared coordination service) WITHOUT touching the engine — that is a
//! documented implementation slot, deliberately not forced here, so board uptime
//! never depends on a distributed lease a single canonical server does not need.
//!
//! # No new dependency
//!
//! The epoch is the load-bearing fence, and a single canonical server needs only
//! single-host exclusion, so the local lease is a lock-*file* (owner metadata +
//! monotonic epoch + pid-liveness) in pure `std` — no advisory-lock crate. A real
//! OS advisory lock would be a `std`-external leaf; it is not needed because safe
//! reclaim is fully expressible with the monotonic epoch alone.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A monotonic fencing token. A strictly higher value supersedes a lower one; a
/// commit stamped with epoch `e` is fenced once the authority's current epoch
/// exceeds `e`. `0` means "no writer has ever taken the lease".
pub type Epoch = u64;

/// The owner-metadata file name for the local lease, written in the directory the
/// lease is given (the engine passes the data ROOT — the always-present
/// `--data-dir` — so the lease never forces early creation of the lazily-created
/// `.arrow-kanban` store dir). Its content is the current [`WriterOwner`] record —
/// identity plus the epoch that owner holds.
pub const OWNER_FILE: &str = "_writer.owner";

/// Errors reading the lease authority. Small and message-carrying so any
/// [`WriterLease`] implementation (a future coordination-service lease included)
/// maps its own failures into it without widening the engine's bound.
#[derive(Debug)]
pub enum LeaseError {
    /// The lease authority could not be reached / read.
    Unavailable(String),
    /// The persisted lease record could not be decoded.
    Corrupt(String),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Unavailable(m) => write!(f, "writer lease unavailable: {m}"),
            LeaseError::Corrupt(m) => write!(f, "writer lease record corrupt: {m}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// A configurable writer-lease authority.
///
/// The engine holds one of these and, at the commit boundary, asks
/// [`is_current`](WriterLease::is_current) before it makes any write durable. A
/// `false` there means a newer writer has taken the lease and this holder is
/// fenced. The epoch it holds ([`epoch`](WriterLease::epoch)) is stamped on every
/// commit, so the durable log records which writer generation produced each
/// event.
///
/// Kept intentionally small and vocabulary-neutral: acquisition is the concrete
/// type's constructor (it *produces* a held lease value — see
/// [`LocalWriterLease::acquire`]), and this trait is the *held* lease: read the
/// held/current epoch, re-assert via [`renew`](WriterLease::renew).
pub trait WriterLease {
    /// The fencing token this holder currently owns — stamped on every commit.
    fn epoch(&self) -> Epoch;

    /// The authority's current epoch. A value greater than [`epoch`](Self::epoch)
    /// means a newer writer has taken the lease and this holder is fenced.
    fn current_epoch(&self) -> Result<Epoch, LeaseError>;

    /// Whether the epoch this holder owns is still the authority's current one —
    /// the check the commit boundary makes to fence a stale writer. Default:
    /// current epoch equals the held epoch (a diverged authority fails closed).
    fn is_current(&self) -> Result<bool, LeaseError> {
        Ok(self.current_epoch()? == self.epoch())
    }

    /// Re-acquire the lease, adopting a fresh strictly-higher epoch — how a writer
    /// that was fenced (or wants to reassert) becomes current again. Returns the
    /// new held epoch.
    fn renew(&mut self) -> Result<Epoch, LeaseError>;
}

/// The persisted lease record: the current owner's identity and the epoch it
/// holds. Public because "who currently holds the lease" is legitimately
/// inspectable by an operator/tool; it is also the on-disk contract the local
/// lease reads and writes (atomic tmp+rename).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterOwner {
    /// The epoch this owner holds — the monotonic fencing token.
    pub epoch: Epoch,
    /// The owning process id (used for crash-recovery liveness on the local host).
    pub pid: u32,
    /// The owning host (recorded for audit; the local lease assumes single-host).
    pub hostname: String,
    /// When this owner acquired the lease (millis since the unix epoch).
    pub started_at_ms: u64,
    /// A per-acquisition nonce, so two acquisitions by the same pid are distinct.
    pub nonce: u64,
}

/// How an [`acquire`](LocalWriterLease::acquire) resolved — for logging and for
/// the "never silently steal from a live owner" guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquisition {
    /// No prior owner — a fresh lease at epoch 1.
    Fresh,
    /// The prior owner was not alive; reclaimed cleanly (crash recovery).
    Reclaimed { from_epoch: Epoch },
    /// The prior owner was ALIVE; a contested takeover — the incumbent will be
    /// fenced at its next commit. Announced loudly, so it is never a silent steal.
    Contested { from_epoch: Epoch },
    /// The bump could not be durably recorded (read-only / full store), so the
    /// prior epoch is held WITHOUT taking the lease. The write-durability gate
    /// governs writes on an unwritable store; the lease does not self-fence there.
    NonDurable { held: Epoch },
}

/// The shipped default lease: single-host, pure-`std`, owner-metadata + monotonic
/// epoch + crash recovery.
///
/// One record ([`OWNER_FILE`]) in the lease directory holds the current owner and
/// its epoch. [`acquire`](Self::acquire) reads it, mints `prior + 1`, and writes
/// itself as the new owner — bumping (and thereby fencing any incumbent). A dead
/// owner is reclaimed cleanly; a live owner is taken over *loudly* (never
/// silently), because the bump fences it at its next commit regardless.
pub struct LocalWriterLease {
    dir: PathBuf,
    held: Epoch,
    owner: WriterOwner,
    acquisition: Acquisition,
}

impl LocalWriterLease {
    /// Take the lease for the store directory `dir`, minting a strictly-higher
    /// epoch than any prior owner. Infallible by design: if the bump cannot be
    /// persisted (an unwritable store), it holds the prior epoch
    /// ([`Acquisition::NonDurable`]) rather than failing engine construction —
    /// the write-durability gate refuses writes on such a store anyway.
    pub fn acquire(dir: &Path) -> Self {
        Self::acquire_with(dir, &process_is_alive)
    }

    /// [`acquire`](Self::acquire) with an injectable liveness predicate, so the
    /// dead-owner-reclaim vs live-owner-contest branches are testable without
    /// depending on real process ids.
    fn acquire_with(dir: &Path, is_alive: &dyn Fn(u32) -> bool) -> Self {
        let prior = read_owner(dir);
        let prior_epoch = prior.as_ref().map(|o| o.epoch).unwrap_or(0);
        let new_epoch = prior_epoch + 1;
        let me = WriterOwner {
            epoch: new_epoch,
            pid: std::process::id(),
            hostname: hostname(),
            started_at_ms: now_ms(),
            nonce: fresh_nonce(),
        };

        match write_owner(dir, &me) {
            Ok(()) => {
                // The bump is durable — we hold the lease. Classify the takeover
                // (and announce a contested one loudly) only now that it is real.
                let acquisition = match prior.as_ref() {
                    None => Acquisition::Fresh,
                    Some(o) if is_alive(o.pid) => {
                        eprintln!(
                            "writer-lease: CONTESTED takeover — a live owner (pid {}, epoch {}) \
                             still holds the lease; taking it at epoch {new_epoch}. The prior owner \
                             will be FENCED at its next commit (this is not a silent steal).",
                            o.pid, o.epoch
                        );
                        Acquisition::Contested {
                            from_epoch: o.epoch,
                        }
                    }
                    Some(o) => Acquisition::Reclaimed {
                        from_epoch: o.epoch,
                    },
                };
                LocalWriterLease {
                    dir: dir.to_path_buf(),
                    held: new_epoch,
                    owner: me,
                    acquisition,
                }
            }
            Err(_) => {
                // Could not persist the bump (read-only / full store). Hold the
                // prior epoch: is_current then reads the unchanged authority and
                // sees held == current, so we do NOT self-fence on an unwritable
                // store — the write-durability gate is what refuses writes there.
                LocalWriterLease {
                    dir: dir.to_path_buf(),
                    held: prior_epoch,
                    owner: WriterOwner {
                        epoch: prior_epoch,
                        ..me
                    },
                    acquisition: Acquisition::NonDurable { held: prior_epoch },
                }
            }
        }
    }

    /// How the most recent acquisition resolved.
    pub fn acquisition(&self) -> &Acquisition {
        &self.acquisition
    }

    /// This holder's recorded identity (epoch + owner metadata).
    pub fn owner(&self) -> &WriterOwner {
        &self.owner
    }

    /// The lease record currently persisted for `dir` (who holds it right now),
    /// or `None` if no owner is recorded / it cannot be read.
    pub fn current_owner(dir: &Path) -> Option<WriterOwner> {
        read_owner(dir)
    }
}

impl WriterLease for LocalWriterLease {
    fn epoch(&self) -> Epoch {
        self.held
    }

    fn current_epoch(&self) -> Result<Epoch, LeaseError> {
        read_current_epoch(&self.dir)
    }

    fn renew(&mut self) -> Result<Epoch, LeaseError> {
        let cur = read_current_epoch(&self.dir)?;
        let next = cur + 1;
        let me = WriterOwner {
            epoch: next,
            pid: std::process::id(),
            hostname: hostname(),
            started_at_ms: now_ms(),
            nonce: fresh_nonce(),
        };
        write_owner(&self.dir, &me).map_err(|e| LeaseError::Unavailable(e.to_string()))?;
        self.held = next;
        self.owner = me;
        self.acquisition = Acquisition::Reclaimed { from_epoch: cur };
        Ok(next)
    }
}

// ── persistence helpers ──────────────────────────────────────────────────────

/// Read the persisted owner record (best-effort; `None` on absent/unreadable/
/// undecodable — the acquire path treats any of these as "no prior owner").
fn read_owner(dir: &Path) -> Option<WriterOwner> {
    let s = fs::read_to_string(dir.join(OWNER_FILE)).ok()?;
    serde_json::from_str(&s).ok()
}

/// Read just the current epoch — the fence read. An ABSENT record is `Ok(0)`
/// (no writer yet); a PRESENT-but-undecodable record is `Err(Corrupt)` so the
/// commit boundary fails closed rather than treating corruption as "current".
fn read_current_epoch(dir: &Path) -> Result<Epoch, LeaseError> {
    let path = dir.join(OWNER_FILE);
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<WriterOwner>(&s)
            .map(|o| o.epoch)
            .map_err(|e| LeaseError::Corrupt(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(LeaseError::Unavailable(format!("{}: {e}", path.display()))),
    }
}

/// Atomically write the owner record via tmp+rename + fsync — durable enough that
/// a crash mid-write never leaves a half record (the rename is atomic; a crash
/// before it leaves the previous complete record).
fn write_owner(dir: &Path, owner: &WriterOwner) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec(owner)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let path = dir.join(OWNER_FILE);
    let tmp = dir.join(format!("{OWNER_FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    if let Ok(dirf) = fs::File::open(dir) {
        let _ = dirf.sync_all();
    }
    Ok(())
}

// ── host / time / liveness primitives (pure std) ─────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A per-acquisition nonce: nanosecond time mixed with a process-local counter,
/// so two acquisitions in the same millisecond by the same pid still differ.
fn fresh_nonce() -> u64 {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    t ^ c.rotate_left(32)
}

/// Best-effort host name (cached once). Recorded for audit; the local lease
/// assumes a single canonical host, so liveness is a local pid check.
fn hostname() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "localhost".to_string())
    })
    .clone()
}

/// Whether a process id is alive on this host — the crash-recovery signal. Pure
/// `std`: `kill -0 <pid>` succeeds iff the process exists (single-user host).
/// If liveness cannot be determined, assume ALIVE — the conservative choice, so
/// an uncertain owner is never reclaimed (and the epoch bump still fences it).
fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::process::{Command, Stdio};
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(true)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_dead(_pid: u32) -> bool {
        false
    }
    fn always_alive(_pid: u32) -> bool {
        true
    }

    /// A fresh directory yields epoch 1 and a `Fresh` acquisition.
    #[test]
    fn fresh_acquire_starts_at_epoch_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease = LocalWriterLease::acquire(dir.path());
        assert_eq!(lease.epoch(), 1);
        assert_eq!(lease.acquisition(), &Acquisition::Fresh);
        assert!(lease.is_current().expect("current"));
    }

    /// A second acquisition bumps the epoch and FENCES the first: the first
    /// holder still reports its held epoch, but `is_current` is now false, while
    /// the second is current. This is the fence, at the lease layer.
    #[test]
    fn a_second_acquire_bumps_and_fences_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = LocalWriterLease::acquire(dir.path());
        assert_eq!(first.epoch(), 1);
        assert!(first.is_current().expect("current"));

        // A live-owner takeover (the same process is alive) → Contested, epoch 2.
        let second = LocalWriterLease::acquire_with(dir.path(), &always_alive);
        assert_eq!(second.epoch(), 2);
        assert_eq!(
            second.acquisition(),
            &Acquisition::Contested { from_epoch: 1 }
        );

        // The first is fenced (held 1, authority now 2); the second is current.
        assert!(
            !first.is_current().expect("first currency"),
            "the first holder must be fenced once the epoch moved past it"
        );
        assert_eq!(first.epoch(), 1, "a held epoch is immutable");
        assert!(second.is_current().expect("second currency"));
    }

    /// Crash recovery: a DEAD owner is reclaimed cleanly with a higher epoch.
    #[test]
    fn a_dead_owner_is_reclaimed_with_a_higher_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Plant a prior owner at epoch 5 whose pid is (per the injected predicate) dead.
        write_owner(
            dir.path(),
            &WriterOwner {
                epoch: 5,
                pid: 999_999,
                hostname: "old-host".into(),
                started_at_ms: 1,
                nonce: 1,
            },
        )
        .expect("plant owner");

        let lease = LocalWriterLease::acquire_with(dir.path(), &always_dead);
        assert_eq!(lease.epoch(), 6, "reclaim mints a strictly higher epoch");
        assert_eq!(
            lease.acquisition(),
            &Acquisition::Reclaimed { from_epoch: 5 }
        );
        assert!(lease.is_current().expect("current after reclaim"));
    }

    /// A LIVE owner is never *silently* stolen from: the takeover is classified
    /// Contested (announced), and the incumbent — a holder still on epoch 5 —
    /// ends up fenced (authority moved to 6). Contrast with the dead case above.
    #[test]
    fn a_live_owner_is_a_contested_takeover_not_a_silent_steal() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_owner(
            dir.path(),
            &WriterOwner {
                epoch: 5,
                pid: std::process::id(),
                hostname: hostname(),
                started_at_ms: 1,
                nonce: 1,
            },
        )
        .expect("plant owner");

        let lease = LocalWriterLease::acquire_with(dir.path(), &always_alive);
        assert_eq!(
            lease.acquisition(),
            &Acquisition::Contested { from_epoch: 5 }
        );
        assert_eq!(lease.epoch(), 6);
        // The incumbent (still on epoch 5) is fenced: the authority reads 6 now.
        assert_eq!(read_current_epoch(dir.path()).expect("read"), 6);
    }

    /// On an unwritable store the acquire cannot record its bump, so it holds the
    /// prior epoch (NonDurable) and does NOT self-fence — is_current stays true
    /// because the authority is unchanged. The durability gate governs writes.
    #[cfg(unix)]
    #[test]
    fn a_nonwritable_store_holds_the_prior_epoch_without_self_fencing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        write_owner(
            dir.path(),
            &WriterOwner {
                epoch: 3,
                pid: std::process::id(),
                hostname: hostname(),
                started_at_ms: 1,
                nonce: 1,
            },
        )
        .expect("plant owner");
        let mut perms = fs::metadata(dir.path()).expect("meta").permissions();
        perms.set_mode(0o500); // r-x: cannot create the tmp record
        fs::set_permissions(dir.path(), perms).expect("chmod");

        let lease = LocalWriterLease::acquire_with(dir.path(), &always_alive);

        // restore perms before assertions so the tempdir cleans up
        let mut back = fs::metadata(dir.path()).expect("meta").permissions();
        back.set_mode(0o700);
        fs::set_permissions(dir.path(), back).expect("chmod back");

        assert_eq!(lease.acquisition(), &Acquisition::NonDurable { held: 3 });
        assert_eq!(lease.epoch(), 3, "holds the prior epoch, did not bump");
        assert!(
            lease.is_current().expect("current"),
            "must not self-fence on an unwritable store"
        );
    }

    /// A fenced holder becomes current again by renewing (re-acquiring a higher
    /// epoch).
    #[test]
    fn renew_reasserts_a_higher_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut first = LocalWriterLease::acquire(dir.path());
        // Someone else takes it (epoch 2) — first is fenced.
        let _second = LocalWriterLease::acquire_with(dir.path(), &always_alive);
        assert!(!first.is_current().expect("fenced"));

        let e = first.renew().expect("renew");
        assert_eq!(e, 3);
        assert_eq!(first.epoch(), 3);
        assert!(
            first.is_current().expect("current after renew"),
            "renewing re-asserts the lease at a higher epoch"
        );
    }

    /// A corrupt authority record fails the fence read closed (Err), so the
    /// commit boundary refuses rather than treating corruption as current.
    #[test]
    fn a_corrupt_record_fails_the_fence_read_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease = LocalWriterLease::acquire(dir.path());
        fs::write(dir.path().join(OWNER_FILE), b"{ not json").expect("corrupt");
        assert!(
            lease.current_epoch().is_err(),
            "a corrupt record must not read as a valid epoch"
        );
        assert!(
            lease.is_current().is_err(),
            "is_current propagates the error"
        );
    }

    /// The REAL liveness detector recognises a dead child pid and a live self —
    /// so the injected-predicate tests above are anchored to real behaviour.
    #[cfg(unix)]
    #[test]
    fn real_liveness_detects_a_dead_child_and_a_live_self() {
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn child");
        let dead_pid = child.id();
        let mut child = child;
        child.wait().expect("reap child");
        assert!(
            !process_is_alive(dead_pid),
            "an exited child pid must read as dead"
        );
        assert!(
            process_is_alive(std::process::id()),
            "the running process must read as alive"
        );
    }
}
