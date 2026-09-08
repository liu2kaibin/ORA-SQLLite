use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use super::Lsn;
use crate::{DbError, ErrorCode, Result};

#[derive(Debug, Default)]
struct SnapshotCounts {
    by_lsn: BTreeMap<Lsn, usize>,
    invalid: bool,
}

#[derive(Debug, Default)]
pub(super) struct SnapshotRegistry {
    counts: Arc<Mutex<SnapshotCounts>>,
}

impl SnapshotRegistry {
    pub(super) fn register(&self, lsn: Lsn) -> Result<SnapshotLease> {
        let mut counts = self.lock_counts()?;
        let count = counts.by_lsn.get(&lsn).copied().unwrap_or(0);
        let Some(next_count) = count.checked_add(1) else {
            counts.invalid = true;
            return Err(internal("snapshot reference count overflow"));
        };
        counts.by_lsn.insert(lsn, next_count);
        drop(counts);

        Ok(SnapshotLease {
            counts: self.counts.clone(),
            lsn,
        })
    }

    pub(super) fn oldest(&self) -> Result<Option<Lsn>> {
        let counts = self.lock_counts()?;
        Ok(counts.by_lsn.first_key_value().map(|(&lsn, _)| lsn))
    }

    pub(super) fn is_empty(&self) -> Result<bool> {
        let counts = self.lock_counts()?;
        Ok(counts.by_lsn.is_empty())
    }

    pub(super) fn lock_empty(&self) -> Result<EmptySnapshotGuard<'_>> {
        let counts = self.lock_counts()?;
        if !counts.by_lsn.is_empty() {
            return Err(DbError::new(
                ErrorCode::Busy,
                "active snapshots prevent resetting the WAL",
            ));
        }
        Ok(EmptySnapshotGuard { _counts: counts })
    }

    fn lock_counts(&self) -> Result<MutexGuard<'_, SnapshotCounts>> {
        let counts = self
            .counts
            .lock()
            .map_err(|_| internal("snapshot registry mutex is poisoned"))?;
        if counts.invalid {
            return Err(internal("snapshot registry state is invalid"));
        }
        Ok(counts)
    }
}

#[derive(Debug)]
pub(super) struct SnapshotLease {
    counts: Arc<Mutex<SnapshotCounts>>,
    lsn: Lsn,
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        let Ok(mut counts) = self.counts.lock() else {
            return;
        };
        if counts.invalid {
            return;
        }

        match counts.by_lsn.get_mut(&self.lsn) {
            Some(count) if *count > 1 => *count -= 1,
            Some(count) if *count == 1 => {
                counts.by_lsn.remove(&self.lsn);
            }
            Some(_) | None => counts.invalid = true,
        }
    }
}

#[derive(Debug)]
pub(super) struct EmptySnapshotGuard<'a> {
    _counts: MutexGuard<'a, SnapshotCounts>,
}

fn internal(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::TryLockError,
        thread,
    };

    use super::{SnapshotLease, SnapshotRegistry};
    use crate::{ErrorCode, storage::Lsn};

    #[test]
    fn duplicate_snapshot_lsns_remain_registered_until_the_last_lease_drops() {
        let registry = SnapshotRegistry::default();
        assert!(registry.is_empty().expect("new registry"));

        let first_three = registry.register(Lsn(3)).expect("first LSN 3");
        let second_three = registry.register(Lsn(3)).expect("second LSN 3");
        let eight = registry.register(Lsn(8)).expect("LSN 8");
        {
            let counts = registry.counts.lock().expect("snapshot counts");
            assert_eq!(counts.by_lsn.get(&Lsn(3)), Some(&2));
            assert_eq!(counts.by_lsn.get(&Lsn(8)), Some(&1));
        }
        assert_eq!(registry.oldest().expect("oldest"), Some(Lsn(3)));

        drop(first_three);
        assert_eq!(
            registry
                .counts
                .lock()
                .expect("snapshot counts")
                .by_lsn
                .get(&Lsn(3)),
            Some(&1)
        );
        assert_eq!(registry.oldest().expect("duplicate remains"), Some(Lsn(3)));
        drop(eight);
        assert!(
            !registry
                .counts
                .lock()
                .expect("snapshot counts")
                .by_lsn
                .contains_key(&Lsn(8))
        );
        assert_eq!(registry.oldest().expect("older remains"), Some(Lsn(3)));
        drop(second_three);
        assert!(
            !registry
                .counts
                .lock()
                .expect("snapshot counts")
                .by_lsn
                .contains_key(&Lsn(3))
        );
        assert_eq!(registry.oldest().expect("no oldest"), None);
        assert!(registry.is_empty().expect("all leases dropped"));
    }

    #[test]
    fn snapshot_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<SnapshotRegistry>();
        assert_send_sync::<SnapshotLease>();
    }

    #[test]
    fn nonempty_registry_cannot_be_locked_for_wal_reset() {
        let registry = SnapshotRegistry::default();
        let _lease = registry.register(Lsn(4)).expect("snapshot");

        assert_eq!(
            registry
                .lock_empty()
                .expect_err("active snapshot must prevent WAL reset")
                .code(),
            ErrorCode::Busy
        );
    }

    #[test]
    fn empty_guard_holds_the_registration_mutex_until_release() {
        let registry = SnapshotRegistry::default();
        let guard = registry.lock_empty().expect("empty registry guard");

        assert!(matches!(
            registry.counts.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(guard);

        let lease = registry.register(Lsn(9)).expect("register after guard");
        assert_eq!(registry.oldest().expect("registered LSN"), Some(Lsn(9)));
        drop(lease);
    }

    #[test]
    fn snapshot_count_overflow_returns_internal_and_invalidates_the_registry() {
        let registry = SnapshotRegistry::default();
        registry
            .counts
            .lock()
            .expect("snapshot counts")
            .by_lsn
            .insert(Lsn(3), usize::MAX);

        assert_eq!(
            registry
                .register(Lsn(3))
                .expect_err("count overflow")
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(
            registry.oldest().expect_err("invalid registry").code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn poisoned_registry_operations_return_internal() {
        let registry = SnapshotRegistry::default();
        let counts = registry.counts.clone();
        let _ = thread::spawn(move || {
            let _guard = counts.lock().expect("snapshot counts");
            panic!("poison snapshot registry");
        })
        .join();

        assert_eq!(
            registry
                .register(Lsn(1))
                .expect_err("poisoned register")
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(
            registry.oldest().expect_err("poisoned oldest").code(),
            ErrorCode::Internal
        );
        assert_eq!(
            registry.is_empty().expect_err("poisoned emptiness").code(),
            ErrorCode::Internal
        );
        assert_eq!(
            registry.lock_empty().expect_err("poisoned guard").code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn missing_count_during_drop_does_not_panic_and_invalidates_the_registry() {
        let registry = SnapshotRegistry::default();
        let lease = registry.register(Lsn(6)).expect("snapshot");
        registry
            .counts
            .lock()
            .expect("snapshot counts")
            .by_lsn
            .remove(&Lsn(6));

        assert!(catch_unwind(AssertUnwindSafe(|| drop(lease))).is_ok());
        assert_eq!(
            registry.is_empty().expect_err("invalid registry").code(),
            ErrorCode::Internal
        );
    }
}
