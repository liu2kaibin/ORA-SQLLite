use super::{
    Lsn,
    pager::{PagerInner, WriteState},
    wal::WAL_HEADER_AREA_LEN,
};
use crate::{DbError, ErrorCode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CheckpointResult {
    pub(super) from_lsn: Lsn,
    pub(super) to_lsn: Lsn,
    pub(super) pages_written: u64,
    pub(super) wal_reset: bool,
}

/// Caller retains the writer mutex throughout materialization and both publications.
pub(super) fn run(inner: &PagerInner) -> Result<CheckpointResult> {
    let operation = || -> Result<CheckpointResult> {
        let (checkpoint, latest, target, state, versions) = {
            let published = inner.published.read().map_err(|_| lock_error())?;
            if published.write_state == WriteState::NeedsRecovery {
                return Err(DbError::new(
                    ErrorCode::Transaction,
                    "checkpoint requires WAL recovery",
                ));
            }
            let target = inner.snapshots.oldest()?.unwrap_or(published.latest_lsn);
            let state = published
                .wal_index
                .state_at(&published.checkpoint, target)?;
            let versions = published
                .wal_index
                .checkpoint_versions(published.checkpoint.checkpoint_lsn(), target);
            (
                published.checkpoint,
                published.latest_lsn,
                target,
                state,
                versions,
            )
        };
        let mut result = CheckpointResult {
            from_lsn: checkpoint.checkpoint_lsn(),
            to_lsn: target,
            pages_written: u64::try_from(versions.len()).map_err(|_| lock_error())?,
            wal_reset: false,
        };
        if target > checkpoint.checkpoint_lsn() {
            let mut candidate = checkpoint.with_checkpoint(
                target,
                state.page_count,
                state.catalog_root,
                state.freelist_root,
                checkpoint.wal_generation(),
            )?;
            candidate.set_features(state.feature_bits)?;
            let mut main = inner.main.lock().map_err(|_| lock_error())?;
            main.extend_to_pages(state.page_count)?;
            for (page_id, version) in versions {
                let frame = inner
                    .wal
                    .lock()
                    .map_err(|_| lock_error())?
                    .read_page_frame(page_id, version, checkpoint.page_size())?;
                main.write_page(page_id, frame.bytes())?;
            }
            main.sync_all()?;
            main.publish_superblock(candidate)?;
            drop(main);
            let mut published = inner.published.write().map_err(|_| lock_error())?;
            published.checkpoint = candidate;
            published.wal_index.discard_through(target);
        }

        if target == latest
            && inner.wal.lock().map_err(|_| lock_error())?.len() > WAL_HEADER_AREA_LEN
        {
            // Same order as begin_read: published -> registry. Keep both until reset is done.
            let mut published = inner.published.write().map_err(|_| lock_error())?;
            let empty = match inner.snapshots.lock_empty() {
                Ok(empty) => empty,
                Err(error) if error.code() == ErrorCode::Busy => return Ok(result),
                Err(error) => return Err(error),
            };
            let mut wal = inner.wal.lock().map_err(|_| lock_error())?;
            let reset = wal.begin_generation_reset(latest)?;
            let mut candidate = published.checkpoint.with_checkpoint(
                latest,
                state.page_count,
                state.catalog_root,
                state.freelist_root,
                reset.generation(),
            )?;
            candidate.set_features(state.feature_bits)?;
            inner
                .main
                .lock()
                .map_err(|_| lock_error())?
                .publish_superblock(candidate)?;
            published.checkpoint = candidate;
            wal.finish_generation_reset(reset, &candidate)?;
            published.wal_index.discard_through(latest);
            // Handles are immutable and may outlive their snapshots; removing cache entries
            // never invalidates those handles.
            let mut cache = inner.cache.lock().map_err(|_| lock_error())?;
            *cache = super::cache::PageCache::new(inner.cache_pages)?;
            drop(cache);
            result.wal_reset = true;
            drop(wal);
            drop(empty);
        }
        Ok(result)
    };
    let result = operation();
    if result.is_err() {
        if let Ok(mut published) = inner.published.write() {
            published.write_state = WriteState::NeedsRecovery;
        }
    }
    result
}

fn lock_error() -> DbError {
    DbError::new(
        ErrorCode::Internal,
        "checkpoint state lock or count is invalid",
    )
}

#[cfg(test)]
mod tests {
    use crate::storage::{
        Lsn, PageRef,
        features::CATALOG_V1,
        file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode},
        page::PageType,
        pager::{Pager, PagerConfig},
        test_support::TempDatabase,
        wal::wal_path,
    };
    use crate::{
        DbError, ErrorCode, Result,
        fault::{FaultInjector, FaultPoint},
    };
    use std::sync::{Arc, Mutex};

    fn append(pager: &Pager, value: u8) -> PageRef {
        let mut batch = pager.begin_write().expect("writer");
        let page = batch.allocate_append(PageType::Catalog).expect("page");
        batch.modify(page).expect("modify").bytes_mut()[100] = value;
        batch.commit().expect("commit");
        page
    }

    fn update(pager: &Pager, page: PageRef, value: u8) {
        let mut batch = pager.begin_write().unwrap();
        batch.modify(page).unwrap().bytes_mut()[100] = value;
        batch.commit().unwrap();
    }

    #[test]
    fn oldest_reader_bounds_checkpoint_and_release_allows_reset() {
        let temp = TempDatabase::new("checkpoint-readers");
        let mut config = PagerConfig::test_default(4096);
        config.cache_pages = 1; // Force subsequent reads to load from their actual source.
        let pager = Pager::create(temp.path(), config).unwrap();
        let page = append(&pager, 11);
        update(&pager, page, 22);
        let mut old = pager.begin_read().unwrap();
        update(&pager, page, 33);
        let other = append(&pager, 44);
        let mut latest = pager.begin_read().unwrap();
        let wal_len = std::fs::metadata(wal_path(temp.path())).unwrap().len();
        let first = pager.checkpoint().unwrap();
        assert_eq!(
            (
                first.from_lsn,
                first.to_lsn,
                first.pages_written,
                first.wal_reset
            ),
            (Lsn(0), Lsn(2), 1, false)
        );
        assert_eq!(
            std::fs::metadata(wal_path(temp.path())).unwrap().len(),
            wal_len
        );
        assert_eq!(latest.read(other).unwrap().bytes()[100], 44);
        assert_eq!(old.read(page).unwrap().bytes()[100], 22);
        assert_eq!(latest.read(page).unwrap().bytes()[100], 33);
        let again = pager.checkpoint().unwrap();
        assert_eq!(
            (
                again.from_lsn,
                again.to_lsn,
                again.pages_written,
                again.wal_reset
            ),
            (Lsn(2), Lsn(2), 0, false)
        );
        assert_eq!(pager.checkpoint().unwrap(), again);
        drop(old);
        // A latest reader still prevents physical reset, but not checkpoint publication.
        let held = pager.checkpoint().unwrap();
        assert_eq!(
            (held.to_lsn, held.pages_written, held.wal_reset),
            (Lsn(4), 2, false)
        );
        assert_eq!(latest.read(page).unwrap().bytes()[100], 33);
        assert_eq!(latest.read(other).unwrap().bytes()[100], 44);
        drop(latest);
        let reset = pager.checkpoint().unwrap();
        assert_eq!(
            (
                reset.from_lsn,
                reset.to_lsn,
                reset.pages_written,
                reset.wal_reset
            ),
            (Lsn(4), Lsn(4), 0, true)
        );
        let quiet = pager.checkpoint().unwrap();
        assert_eq!((quiet.pages_written, quiet.wal_reset), (0, false));
        assert_eq!(pager.checkpoint().unwrap(), quiet);
        drop(pager);
        let main = DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
            .unwrap();
        assert_eq!(main.header().wal_generation(), 2);
    }

    #[derive(Default)]
    struct CheckpointFaults {
        state: Mutex<(bool, Option<usize>, Vec<FaultPoint>)>,
    }

    impl CheckpointFaults {
        fn arm(&self, fail_at: Option<usize>) {
            *self.state.lock().unwrap() = (true, fail_at, Vec::new());
        }
        fn recorded(&self) -> Vec<FaultPoint> {
            self.state.lock().unwrap().2.clone()
        }
    }

    impl FaultInjector for CheckpointFaults {
        fn check(&self, point: FaultPoint) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if state.0 {
                state.2.push(point);
                if state.1 == Some(state.2.len()) {
                    return Err(DbError::new(ErrorCode::Io, "checkpoint boundary fault"));
                }
            }
            Ok(())
        }
    }

    #[test]
    fn every_checkpoint_io_fault_reopens_a_complete_commit() {
        let recording_temp = TempDatabase::new("checkpoint-record");
        let recording = Arc::new(CheckpointFaults::default());
        let pager = Pager::create(recording_temp.path(), PagerConfig::test_default(4096))
            .unwrap()
            .with_faults(recording.clone());
        append(&pager, 11);
        append(&pager, 22);
        recording.arm(None);
        pager.checkpoint().unwrap();
        let points = recording.recorded();
        assert_eq!(
            points
                .iter()
                .map(|point| format!("{point:?}"))
                .collect::<Vec<_>>(),
            vec![
                "BeforeMainExtend",
                "AfterMainExtend",
                "BeforeMainPageWrite",
                "AfterMainPageWrite",
                "BeforeMainPageWrite",
                "AfterMainPageWrite",
                "BeforeMainSync",
                "AfterMainSync",
                "BeforeSuperblockWrite",
                "AfterSuperblockWrite",
                "BeforeMainSync",
                "AfterMainSync",
                "BeforeWalHeaderWrite",
                "AfterWalHeaderWrite",
                "BeforeWalSync",
                "AfterWalSync",
                "BeforeSuperblockWrite",
                "AfterSuperblockWrite",
                "BeforeMainSync",
                "AfterMainSync",
                "BeforeWalTruncate",
                "AfterWalTruncate",
                "BeforeWalSync",
                "AfterWalSync",
            ]
        );
        drop(pager);

        for (index, point) in points.iter().enumerate() {
            let temp = TempDatabase::new("checkpoint-fault");
            let faults = Arc::new(CheckpointFaults::default());
            let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
                .unwrap()
                .with_faults(faults.clone());
            let first = append(&pager, 11);
            let second = append(&pager, 22);
            faults.arm(Some(index + 1));
            assert_eq!(pager.checkpoint().unwrap_err().code(), ErrorCode::Io);
            assert!(pager.is_poisoned());
            assert_eq!(faults.recorded(), points[..=index]);
            assert_eq!(pager.read(first).unwrap().bytes()[100], 11);
            assert_eq!(pager.read(second).unwrap().bytes()[100], 22);
            drop(pager);
            let reopened = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            )
            .unwrap_or_else(|error| panic!("boundary {} {:?}: {error}", index + 1, point));
            assert_eq!(reopened.latest_lsn().unwrap(), Lsn(2));
            assert_eq!(reopened.read(first).unwrap().bytes()[100], 11);
            assert_eq!(reopened.read(second).unwrap().bytes()[100], 22);
            reopened.checkpoint().unwrap();
            assert_eq!(
                std::fs::metadata(wal_path(temp.path())).unwrap().len(),
                8192
            );
            println!(
                "boundary {} {:?}: complete LSN 2, retry reset OK",
                index + 1,
                point
            );
        }
    }

    #[test]
    fn checkpoint_recovery_rejects_unproven_or_unaligned_main_tail() {
        for len in [8193, 12288, 16384] {
            let temp = TempDatabase::new("checkpoint-invalid-tail");
            let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
            if len == 16384 {
                append(&pager, 11);
            }
            drop(pager);
            std::fs::OpenOptions::new()
                .write(true)
                .open(temp.path())
                .unwrap()
                .set_len(len)
                .unwrap();
            let error = match Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            ) {
                Ok(_) => panic!("unproven tail {len} must fail"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Corruption);
            assert_eq!(std::fs::metadata(temp.path()).unwrap().len(), len);
        }
    }

    #[test]
    fn writer_threshold_resets_before_granting_the_next_batch() {
        let temp = TempDatabase::new("checkpoint-threshold");
        let mut config = PagerConfig::test_default(4096);
        config.checkpoint_wal_bytes = 12_480;
        config.max_wal_bytes = 16_384;
        let pager = Pager::create(temp.path(), config).unwrap();
        let page = append(&pager, 11);
        assert_eq!(
            std::fs::metadata(wal_path(temp.path())).unwrap().len(),
            12_480
        );
        let batch = pager.try_begin_write().unwrap();
        assert_eq!(
            std::fs::metadata(wal_path(temp.path())).unwrap().len(),
            8192
        );
        drop(batch);
        update(&pager, page, 22);
        assert_eq!(pager.latest_lsn().unwrap(), Lsn(2));
        assert_eq!(pager.read(page).unwrap().bytes()[100], 22);
    }

    #[test]
    fn hard_limit_reports_oldest_reader_and_retries_after_release() {
        let temp = TempDatabase::new("checkpoint-hard-limit");
        let mut config = PagerConfig::test_default(4096);
        config.checkpoint_wal_bytes = 12_480;
        config.max_wal_bytes = 16_384;
        let pager = Pager::create(temp.path(), config).unwrap();
        let page = append(&pager, 11);
        let mut snapshot = pager.begin_read().unwrap();
        let before = std::fs::read(wal_path(temp.path())).unwrap();
        {
            let mut batch = pager.begin_write().unwrap();
            batch.modify(page).unwrap().bytes_mut()[100] = 22;
            let error = batch.commit().unwrap_err();
            assert_eq!(error.code(), ErrorCode::Busy);
            assert!(error.to_string().contains("oldest snapshot LSN 1"));
        }
        assert_eq!(std::fs::read(wal_path(temp.path())).unwrap(), before);
        assert_eq!(snapshot.read(page).unwrap().bytes()[100], 11);
        assert!(!pager.is_poisoned());
        drop(snapshot);
        update(&pager, page, 22);
        assert_eq!(pager.read(page).unwrap().bytes()[100], 22);
    }

    #[test]
    fn projected_limit_can_checkpoint_even_below_the_soft_threshold() {
        let temp = TempDatabase::new("checkpoint-projected-limit");
        let mut config = PagerConfig::test_default(4096);
        config.checkpoint_wal_bytes = 1_000_000;
        config.max_wal_bytes = 16_384;
        let pager = Pager::create(temp.path(), config).unwrap();
        let page = append(&pager, 11);
        update(&pager, page, 22);
        assert_eq!(
            std::fs::metadata(wal_path(temp.path())).unwrap().len(),
            12_480
        );
        assert_eq!(pager.read(page).unwrap().bytes()[100], 22);
    }

    #[test]
    fn oversized_transaction_is_rejected_without_wal_io() {
        let temp = TempDatabase::new("checkpoint-oversized-transaction");
        let mut config = PagerConfig::test_default(4096);
        config.checkpoint_wal_bytes = 1_000_000;
        config.max_wal_bytes = 16_384;
        let faults = Arc::new(CheckpointFaults::default());
        let pager = Pager::create(temp.path(), config)
            .unwrap()
            .with_faults(faults.clone());
        let before = std::fs::read(wal_path(temp.path())).unwrap();
        {
            let mut batch = pager.begin_write().unwrap();
            for _ in 0..4 {
                batch.allocate_append(PageType::Catalog).unwrap();
            }
            faults.arm(None);
            assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
        }
        assert!(faults.recorded().is_empty());
        assert_eq!(std::fs::read(wal_path(temp.path())).unwrap(), before);
        assert_eq!(pager.latest_lsn().unwrap(), Lsn(0));
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn invalid_wal_limits_do_not_create_files() {
        for soft in [true, false] {
            let temp = TempDatabase::new("checkpoint-invalid-config");
            let mut config = PagerConfig::test_default(4096);
            if soft {
                config.checkpoint_wal_bytes = 0;
            } else {
                config.max_wal_bytes = 0;
            }
            let error = match Pager::create(temp.path(), config) {
                Ok(_) => panic!("zero limit"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Type);
            assert!(!temp.path().exists());
        }
    }

    #[test]
    fn repeated_resets_preserve_generation_and_consecutive_commits_on_reopen() {
        let temp = TempDatabase::new("checkpoint-generations");
        let mut pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
        let page = append(&pager, 1);
        for value in 1..=4 {
            if value > 1 {
                update(&pager, page, value);
            }
            assert!(pager.checkpoint().unwrap().wal_reset);
            drop(pager);
            let main =
                DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
                    .unwrap();
            assert_eq!(main.header().wal_generation(), u64::from(value) + 1);
            assert_eq!(main.header().checkpoint_lsn(), Lsn(u64::from(value)));
            drop(main);
            pager = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            assert_eq!(pager.read(page).unwrap().bytes()[100], value);
        }
    }

    #[test]
    fn read_only_checkpoint_does_not_change_either_file() {
        let temp = TempDatabase::new("checkpoint-read-only");
        let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
        let page = append(&pager, 11);
        drop(pager);
        let before_main = std::fs::read(temp.path()).unwrap();
        let before_wal = std::fs::read(wal_path(temp.path())).unwrap();
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        assert_eq!(
            pager.checkpoint().unwrap_err().code(),
            ErrorCode::Unsupported
        );
        assert_eq!(pager.read(page).unwrap().bytes()[100], 11);
        assert!(!pager.is_poisoned());
        drop(pager);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before_main);
        assert_eq!(std::fs::read(wal_path(temp.path())).unwrap(), before_wal);
    }

    #[test]
    fn checkpoint_persists_catalog_overflow_and_freelist_roots_at_every_page_size() {
        use crate::storage::{
            btree::{BTree, TreeKind},
            pager::PageSource,
        };
        for page_size in [4096, 8192, 16384] {
            let temp = TempDatabase::new("checkpoint-roots");
            let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size)).unwrap();
            let value = vec![0x73; 20_000];
            let root = {
                let mut batch = pager.begin_write().unwrap();
                let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
                tree.insert(&mut batch, b"checkpoint", &value).unwrap();
                batch.track_tree(TreeKind::Table, tree.root()).unwrap();
                batch.set_catalog_root(tree.root()).unwrap();
                batch.commit().unwrap();
                tree.root()
            };
            let free = append(&pager, 11);
            {
                let mut batch = pager.begin_write().unwrap();
                batch.release_later(free).unwrap();
                batch.commit().unwrap();
            }
            let page_count = pager.page_count();
            assert!(pager.checkpoint().unwrap().wal_reset);
            drop(pager);
            let main =
                DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
                    .unwrap();
            assert_eq!(main.header().catalog_root(), root.page_id());
            assert_eq!(main.header().freelist_root(), free.page_id());
            assert_eq!(main.header().page_count(), page_count);
            assert_eq!(main.header().checkpoint_lsn(), Lsn(3));
            drop(main);
            let reopened = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(page_size),
            )
            .unwrap();
            let mut snapshot = reopened.begin_read().unwrap();
            assert_eq!(snapshot.freelist_root().unwrap(), free.page_id());
            let tree = snapshot.catalog_tree(TreeKind::Table).unwrap().unwrap();
            assert_eq!(tree.get(&mut snapshot, b"checkpoint").unwrap(), Some(value));
        }
    }

    #[test]
    fn checkpoint_without_readers_publishes_latest_and_resets_wal() {
        let temp = TempDatabase::new("checkpoint-latest");
        let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
        let first = append(&pager, 11);
        let second = append(&pager, 22);
        let result = pager.checkpoint().expect("checkpoint");
        assert_eq!(result.from_lsn, Lsn(0));
        assert_eq!(result.to_lsn, Lsn(2));
        assert_eq!(result.pages_written, 2);
        assert!(result.wal_reset);
        assert_eq!(
            std::fs::metadata(wal_path(temp.path())).unwrap().len(),
            8192
        );
        assert_eq!(pager.read(first).unwrap().bytes()[100], 11);
        assert_eq!(pager.read(second).unwrap().bytes()[100], 22);
        drop(pager);
        let mut main =
            DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap();
        assert_eq!(main.header().checkpoint_lsn(), Lsn(2));
        assert_eq!(main.header().wal_generation(), 2);
        assert_eq!(main.read_page(first.page_id()).unwrap()[100], 11);
        assert_eq!(main.read_page(second.page_id()).unwrap()[100], 22);
        drop(main);
        let reopened = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        assert_eq!(reopened.latest_lsn().unwrap(), Lsn(2));
        assert_eq!(reopened.read(second).unwrap().bytes()[100], 22);
    }

    #[test]
    fn checkpoint_reset_preserves_feature_bits_for_snapshots_and_reopen() {
        let temp = TempDatabase::new("checkpoint-feature-bits");
        let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
        {
            let mut batch = pager.begin_write().unwrap();
            batch.enable_feature_bits(CATALOG_V1).unwrap();
            batch.allocate_append(PageType::Catalog).unwrap();
            batch.commit().unwrap();
        }
        assert_eq!(pager.begin_read().unwrap().feature_bits(), CATALOG_V1);

        let result = pager.checkpoint().unwrap();
        assert!(result.wal_reset);
        assert_eq!(pager.begin_read().unwrap().feature_bits(), CATALOG_V1);

        drop(pager);
        let reopened = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        assert_eq!(reopened.begin_read().unwrap().feature_bits(), CATALOG_V1);
    }
}
