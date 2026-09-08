use std::{
    collections::BTreeMap,
    fs,
    io::{ErrorKind, Write},
    ops::Bound,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    DbError, ErrorCode, Result,
    fault::{FaultInjector, FaultPoint},
};

use super::{
    Lsn, PageId, PageRef,
    btree::{BTree, TreeKind},
    checksum::stamp_page_checksum,
    file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode, checked_page_offset},
    header::{SUPERBLOCK_AREA_LEN, SUPERBLOCK_SLOT_LEN, SuperblockSlot},
    page::{PageFrame, PageType},
    pager::{PageSource, Pager, PagerConfig},
    validate::validate_tree,
    wal::{Wal, wal_path},
};

static NEXT_TEMP_DATABASE: AtomicU64 = AtomicU64::new(0);

pub(super) struct TempDatabase {
    path: PathBuf,
}

impl TempDatabase {
    pub(super) fn new(label: &str) -> Self {
        Self::new_with_sequence(label, || NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed))
    }

    fn new_with_sequence(label: &str, mut next_sequence: impl FnMut() -> u64) -> Self {
        let process_id = std::process::id();
        let label = sanitized_label(label);

        loop {
            let sequence = next_sequence();
            let path = std::env::temp_dir()
                .join(format!("ora-sqllite-{process_id}-{label}-{sequence}.db"));
            if !path.exists() {
                return Self { path };
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl PagerConfig {
    pub(super) fn test_default(page_size: u32) -> Self {
        Self::new(page_size, [0x5a; 16]).expect("test pager config uses a supported page size")
    }
}

pub(super) fn test_pager(page_size: u32) -> Result<(TempDatabase, Pager)> {
    let temp = TempDatabase::new("pager");
    let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    Ok((temp, pager))
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        for path in [&self.path, &wal_path(&self.path)] {
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != ErrorKind::NotFound {
                    // Test cleanup is best effort; a later test must never remove a different path.
                }
            }
        }
    }
}

pub(super) fn write_wal_fixture(database_path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(wal_path(database_path), bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write WAL fixture: {error}")))
}

fn sanitized_label(label: &str) -> String {
    let label: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if label.is_empty() {
        "database".to_owned()
    } else {
        label
    }
}

#[test]
fn temporary_database_skips_a_direct_name_collision_and_removes_only_its_exact_file() {
    let label = "direct-collision";
    let first_sequence = NEXT_TEMP_DATABASE.fetch_add(2, Ordering::Relaxed);
    let retry_sequence = first_sequence
        .checked_add(1)
        .expect("temporary database retry sequence must not overflow");
    let collision = std::env::temp_dir().join(format!(
        "ora-sqllite-{}-{label}-{first_sequence}.db",
        std::process::id(),
    ));
    fs::write(&collision, b"pre-existing collision").expect("create collision file");

    let mut sequences = [first_sequence, retry_sequence].into_iter();
    let database = TempDatabase::new_with_sequence(label, || {
        sequences
            .next()
            .expect("collision retry must consume exactly two candidates")
    });
    let database_path = database.path().to_path_buf();
    assert_eq!(
        database_path,
        std::env::temp_dir().join(format!(
            "ora-sqllite-{}-{label}-{retry_sequence}.db",
            std::process::id()
        )),
        "TempDatabase::new must retry immediately after the occupied first candidate"
    );
    let sentinel = database_path.with_extension("sentinel");
    let wal = wal_path(&database_path);
    assert_ne!(database_path, collision);
    fs::write(&database_path, b"owned database file").expect("create owned file");
    fs::write(&wal, b"owned WAL file").expect("create owned WAL file");
    fs::write(&sentinel, b"unrelated sentinel").expect("create sentinel file");
    drop(database);

    assert!(
        collision.exists(),
        "Drop must not remove a collided filename"
    );
    assert!(
        !database_path.exists(),
        "Drop must remove its exact generated filename"
    );
    assert!(
        !wal.exists(),
        "Drop must remove the WAL paired with its exact generated filename"
    );
    assert!(
        sentinel.exists(),
        "Drop must not remove an adjacent unrelated filename"
    );
    fs::remove_file(collision).expect("remove collision fixture");
    fs::remove_file(sentinel).expect("remove sentinel fixture");
}

#[test]
fn multi_reader_single_writer_matches_reference_model() {
    use std::{
        sync::{Barrier, mpsc},
        thread,
    };

    const FIRST_COMMIT: u64 = 1;
    const FINAL_COMMIT: u64 = 20;

    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Pager>();
    assert_send::<super::pager::ReadSnapshot>();
    assert_send_sync::<super::cache::PageHandle>();

    let temp = TempDatabase::new("multi-reader-single-writer");
    let pager = Arc::new(
        Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("create concurrent pager"),
    );
    let root = create_concurrency_table(&pager).expect("commit LSN-1 fixture");
    assert_eq!(
        pager.latest_lsn().expect("read fixture LSN"),
        Lsn(FIRST_COMMIT)
    );

    let mut old_snapshot = pager.begin_read().expect("open LSN-1 snapshot");
    assert_eq!(old_snapshot.lsn(), Lsn(FIRST_COMMIT));
    let initial_model = BTreeMap::from([
        (b"counter".to_vec(), vec![1]),
        (b"stable".to_vec(), b"unchanged".to_vec()),
    ]);
    let start = Arc::new(Barrier::new(3));
    let (old_commit_tx, old_commit_rx) = mpsc::sync_channel(0);
    let (old_done_tx, old_done_rx) = mpsc::sync_channel(0);
    let (fresh_commit_tx, fresh_commit_rx) = mpsc::sync_channel(0);
    let (fresh_done_tx, fresh_done_rx) = mpsc::sync_channel(0);
    let writer_initial_model = initial_model.clone();

    let (final_model, old_observations, fresh_observations) = thread::scope(|scope| {
        let writer_pager = Arc::clone(&pager);
        let writer_start = Arc::clone(&start);
        let writer = scope.spawn(move || -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
            let mut tree = BTree::from_root(TreeKind::Table, root);
            let mut model = writer_initial_model;
            writer_start.wait();
            for commit in (FIRST_COMMIT + 1)..=FINAL_COMMIT {
                let mut batch = writer_pager.begin_write()?;
                let removed = tree.delete(&mut batch, b"counter")?;
                assert_eq!(removed, model.remove(b"counter".as_slice()));
                let value = vec![u8::try_from(commit).expect("test commit fits u8")];
                tree.insert(&mut batch, b"counter", &value)?;
                batch.track_tree(TreeKind::Table, tree.root())?;
                batch.set_catalog_root(tree.root())?;
                batch.commit()?;
                model.insert(b"counter".to_vec(), value);

                old_commit_tx
                    .send(())
                    .expect("old reader remains available");
                fresh_commit_tx
                    .send((Lsn(commit), model.clone()))
                    .expect("fresh reader remains available");
                old_done_rx.recv().expect("old reader completed commit");
                fresh_done_rx.recv().expect("fresh reader completed commit");
            }
            Ok(model)
        });

        let old_start = Arc::clone(&start);
        let old_reader = scope.spawn(move || -> Result<Vec<BTreeMap<Vec<u8>, Vec<u8>>>> {
            let tree = BTree::from_root(TreeKind::Table, root);
            let mut observations = Vec::new();
            old_start.wait();
            while old_commit_rx.recv().is_ok() {
                let observed = collect_all(&tree, &mut old_snapshot)?.into_iter().collect();
                observations.push(observed);
                old_done_tx.send(()).expect("writer remains available");
            }
            Ok(observations)
        });

        let fresh_pager = Arc::clone(&pager);
        let fresh_start = Arc::clone(&start);
        let fresh_reader = scope.spawn(move || -> Result<Vec<_>> {
            let tree = BTree::from_root(TreeKind::Table, root);
            let mut observations = Vec::new();
            fresh_start.wait();
            while let Ok((expected_lsn, expected_model)) = fresh_commit_rx.recv() {
                let mut snapshot = fresh_pager.begin_read()?;
                let actual_lsn = snapshot.lsn();
                let actual_model: BTreeMap<_, _> =
                    collect_all(&tree, &mut snapshot)?.into_iter().collect();
                observations.push((expected_lsn, actual_lsn, expected_model, actual_model));
                fresh_done_tx.send(()).expect("writer remains available");
            }
            Ok(observations)
        });

        (
            writer
                .join()
                .expect("writer thread must not panic")
                .expect("writer commits"),
            old_reader
                .join()
                .expect("old-reader thread must not panic")
                .expect("old-reader observations"),
            fresh_reader
                .join()
                .expect("fresh-reader thread must not panic")
                .expect("fresh-reader observations"),
        )
    });

    assert_eq!(old_observations.len(), 19);
    for observed in old_observations {
        assert_eq!(observed, initial_model, "LSN-1 snapshot must stay fixed");
    }
    assert_eq!(fresh_observations.len(), 19);
    let mut previous_lsn = Lsn(FIRST_COMMIT);
    for (expected_lsn, actual_lsn, expected_model, actual_model) in fresh_observations {
        assert!(actual_lsn >= previous_lsn, "fresh snapshot LSN regressed");
        assert_eq!(
            actual_lsn, expected_lsn,
            "reader must see the released commit"
        );
        assert_eq!(
            actual_model, expected_model,
            "fresh snapshot diverged from model"
        );
        previous_lsn = actual_lsn;
    }

    let tree = BTree::from_root(TreeKind::Table, root);
    let mut final_snapshot = pager.begin_read().expect("open final snapshot");
    assert_eq!(final_snapshot.lsn(), Lsn(FINAL_COMMIT));
    let final_actual: BTreeMap<_, _> = collect_all(&tree, &mut final_snapshot)
        .expect("scan final snapshot")
        .into_iter()
        .collect();
    assert_eq!(final_actual, final_model);
}

#[test]
fn unfinished_writer_is_busy_readers_progress_and_checkpoint_waits_for_rollback() {
    use std::{sync::mpsc, thread};

    let temp = TempDatabase::new("writer-checkpoint-contention");
    let pager = Arc::new(
        Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("create contention pager"),
    );
    let root = create_concurrency_table(&pager).expect("commit contention fixture");
    let (writer_ready_tx, writer_ready_rx) = mpsc::sync_channel(0);
    let (release_writer_tx, release_writer_rx) = mpsc::sync_channel(0);
    let (checkpoint_go_tx, checkpoint_go_rx) = mpsc::sync_channel(0);
    let (checkpoint_contended_tx, checkpoint_contended_rx) = mpsc::sync_channel(0);
    let (checkpoint_done_tx, checkpoint_done_rx) = mpsc::sync_channel(0);

    thread::scope(|scope| {
        let writer_pager = Arc::clone(&pager);
        let writer = scope.spawn(move || -> Result<()> {
            let mut tree = BTree::from_root(TreeKind::Table, root);
            let mut batch = writer_pager.begin_write()?;
            assert_eq!(tree.delete(&mut batch, b"counter")?, Some(vec![1]));
            tree.insert(&mut batch, b"counter", b"uncommitted")?;
            writer_ready_tx
                .send(())
                .expect("contention driver remains available");
            release_writer_rx
                .recv()
                .expect("contention driver releases writer");
            drop(batch);
            Ok(())
        });

        let checkpoint_pager = Arc::clone(&pager);
        let checkpoint = scope.spawn(move || {
            checkpoint_go_rx.recv().expect("checkpoint is started");
            let result =
                super::pager::checkpoint_after_writer_contention(&checkpoint_pager, || {
                    checkpoint_contended_tx
                        .send(())
                        .expect("contention driver remains available");
                });
            checkpoint_done_tx
                .send(result)
                .expect("contention driver receives checkpoint result");
        });

        writer_ready_rx
            .recv()
            .expect("unfinished writer acquired guard");
        let busy = match pager.try_begin_write() {
            Ok(_) => panic!("second writer must not acquire the active writer guard"),
            Err(error) => error,
        };
        assert_eq!(busy.code(), ErrorCode::Busy);

        let tree = BTree::from_root(TreeKind::Table, root);
        let mut reader = pager
            .begin_read()
            .expect("reader opens beside unfinished writer");
        assert_eq!(reader.lsn(), Lsn(1));
        assert_eq!(
            tree.get(&mut reader, b"counter").expect("reader lookup"),
            Some(vec![1])
        );
        drop(reader);

        checkpoint_go_tx.send(()).expect("start checkpoint attempt");
        checkpoint_contended_rx
            .recv()
            .expect("checkpoint observed the active writer before blocking");
        release_writer_tx
            .send(())
            .expect("roll back unfinished writer");
        writer
            .join()
            .expect("writer thread must not panic")
            .expect("writer rollback");
        let result = checkpoint_done_rx
            .recv()
            .expect("checkpoint completes after writer rollback")
            .expect("checkpoint succeeds");
        assert_eq!(result.to_lsn, Lsn(1));
        checkpoint.join().expect("checkpoint thread must not panic");
    });
}

#[test]
#[ignore = "spawned by database_lock_is_process_wide_and_releases_windows_paths"]
fn database_lock_child() {
    let path = std::env::var_os("ORA_SQLLITE_LOCK_CHILD_PATH")
        .expect("lock child database path is provided");
    let expected =
        std::env::var("ORA_SQLLITE_LOCK_CHILD_EXPECT").expect("lock child expectation is provided");
    let marker_path = std::env::var_os("ORA_SQLLITE_LOCK_CHILD_MARKER_PATH")
        .expect("lock child marker path is provided");
    let nonce =
        std::env::var("ORA_SQLLITE_LOCK_CHILD_NONCE").expect("lock child nonce is provided");
    let opened = Pager::open(
        Path::new(&path),
        OpenMode::ReadWrite,
        PagerConfig::test_default(4096),
    );
    match expected.as_str() {
        "busy" => match opened {
            Ok(_) => panic!("child unexpectedly acquired its parent's database lock"),
            Err(error) => assert_eq!(error.code(), ErrorCode::Busy),
        },
        "open" => drop(opened.expect("child opens database after parent Pager drop")),
        other => panic!("unknown lock child expectation: {other}"),
    }
    let mut marker = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_path)
        .expect("create unique lock child marker");
    marker
        .write_all(format!("{expected}:{nonce}").as_bytes())
        .expect("write lock child marker");
    marker.sync_all().expect("sync lock child marker");
}

#[test]
fn database_lock_is_process_wide_and_releases_windows_paths() {
    use std::process::Command;

    let temp = TempDatabase::new("database-lock-process");
    let database_path = temp.path().to_path_buf();
    let wal = wal_path(&database_path);
    let pager =
        Pager::create(&database_path, PagerConfig::test_default(4096)).expect("create lock owner");
    let executable = std::env::current_exe().expect("locate current test executable");
    let busy_nonce = format!(
        "{}-{}-busy",
        std::process::id(),
        NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let busy_marker = database_path.with_extension(format!("{busy_nonce}.marker"));

    let busy = Command::new(&executable)
        .args([
            "--exact",
            "storage::test_support::database_lock_child",
            "--ignored",
        ])
        .env("ORA_SQLLITE_LOCK_CHILD_PATH", &database_path)
        .env("ORA_SQLLITE_LOCK_CHILD_EXPECT", "busy")
        .env("ORA_SQLLITE_LOCK_CHILD_MARKER_PATH", &busy_marker)
        .env("ORA_SQLLITE_LOCK_CHILD_NONCE", &busy_nonce)
        .status()
        .expect("run lock-contention child");
    assert_eq!(
        busy.code(),
        Some(0),
        "active owner must map child open to Busy"
    );
    verify_and_remove_lock_child_marker(&busy_marker, "busy", &busy_nonce);

    drop(pager);
    let open_nonce = format!(
        "{}-{}-open",
        std::process::id(),
        NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let open_marker = database_path.with_extension(format!("{open_nonce}.marker"));
    let opened = Command::new(executable)
        .args([
            "--exact",
            "storage::test_support::database_lock_child",
            "--ignored",
        ])
        .env("ORA_SQLLITE_LOCK_CHILD_PATH", &database_path)
        .env("ORA_SQLLITE_LOCK_CHILD_EXPECT", "open")
        .env("ORA_SQLLITE_LOCK_CHILD_MARKER_PATH", &open_marker)
        .env("ORA_SQLLITE_LOCK_CHILD_NONCE", &open_nonce)
        .status()
        .expect("run post-drop child");
    assert_eq!(opened.code(), Some(0), "drop must release the process lock");
    verify_and_remove_lock_child_marker(&open_marker, "open", &open_nonce);

    fs::remove_file(&wal).expect("remove exact WAL path after child exit");
    fs::remove_file(&database_path).expect("remove exact database path after child exit");
    assert!(!database_path.exists());
    assert!(!wal.exists());
    assert!(!busy_marker.exists());
    assert!(!open_marker.exists());
}

fn verify_and_remove_lock_child_marker(marker: &Path, mode: &str, nonce: &str) {
    let contents = fs::read_to_string(marker)
        .expect("lock child marker is missing; the exact helper may not have run");
    fs::remove_file(marker).expect("remove exact lock child marker");
    assert_eq!(contents, format!("{mode}:{nonce}"));
}

fn create_concurrency_table(pager: &Pager) -> Result<PageRef> {
    let mut batch = pager.begin_write()?;
    let mut tree = BTree::create(&mut batch, TreeKind::Table)?;
    tree.insert(&mut batch, b"counter", &[1])?;
    tree.insert(&mut batch, b"stable", b"unchanged")?;
    batch.track_tree(TreeKind::Table, tree.root())?;
    batch.set_catalog_root(tree.root())?;
    let root = tree.root();
    batch.commit()?;
    Ok(root)
}

struct RecordingFaults {
    fail_on: Option<(FaultPoint, usize)>,
    observed: Mutex<Vec<FaultPoint>>,
}

impl RecordingFaults {
    fn never() -> Self {
        Self {
            fail_on: None,
            observed: Mutex::new(Vec::new()),
        }
    }

    fn at(point: FaultPoint, occurrence: usize) -> Self {
        Self {
            fail_on: Some((point, occurrence)),
            observed: Mutex::new(Vec::new()),
        }
    }

    fn observed(&self) -> Vec<FaultPoint> {
        self.observed.lock().expect("fault record lock").clone()
    }
}

impl FaultInjector for RecordingFaults {
    fn check(&self, point: FaultPoint) -> Result<()> {
        let mut observed = self.observed.lock().expect("fault record lock");
        observed.push(point);
        let occurrence = observed.iter().filter(|&&seen| seen == point).count();
        if self.fail_on == Some((point, occurrence)) {
            return Err(DbError::new(ErrorCode::Io, "injected pager I/O fault"));
        }
        Ok(())
    }
}

#[test]
fn test_fault_injector_is_used_after_pager_creation() {
    let temp = TempDatabase::new("test-fault-injector");
    let faults = Arc::new(RecordingFaults::at(FaultPoint::BeforeWalWrite, 1));
    let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
        .expect("create pager")
        .with_faults(faults.clone());
    let page = {
        let mut batch = pager.begin_write().expect("begin write");
        let page = batch
            .allocate_append(super::page::PageType::TableLeaf)
            .expect("allocate page");
        assert_eq!(
            batch
                .commit()
                .expect_err("configured fault must fail commit")
                .code(),
            ErrorCode::Io
        );
        page
    };

    assert_eq!(faults.observed(), vec![FaultPoint::BeforeWalWrite]);
    assert!(pager.is_poisoned());
    assert_eq!(
        pager
            .read(page)
            .expect_err("unpublished page remains invisible")
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn every_wal_commit_fault_requires_recovery_after_a_multi_page_transaction() {
    let baseline_faults = Arc::new(RecordingFaults::never());
    let baseline = commit_multi_page_tree(baseline_faults.clone()).expect("baseline commit");
    assert_eq!(
        baseline,
        vec![
            FaultPoint::BeforeWalWrite,
            FaultPoint::AfterWalWrite,
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
        ],
        "a multi-page transaction must append and sync through the WAL once"
    );

    for point in [
        FaultPoint::BeforeWalWrite,
        FaultPoint::AfterWalWrite,
        FaultPoint::BeforeWalSync,
        FaultPoint::AfterWalSync,
    ] {
        let occurrences = baseline.iter().filter(|&&seen| seen == point).count();
        for occurrence in 1..=occurrences {
            let faults = Arc::new(RecordingFaults::at(point, occurrence));
            let temp = TempDatabase::new(&format!("multi-page-fault-{point:?}-{occurrence}"));
            let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
                .expect("create fault pager")
                .with_faults(faults.clone());
            let root = {
                let mut batch = pager.begin_write().expect("begin multi-page batch");
                let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
                insert_multi_page_fixture(&mut tree, &mut batch).expect("stage tree pages");
                batch
                    .track_tree(TreeKind::Table, tree.root())
                    .expect("track tree");
                batch.set_catalog_root(tree.root()).expect("publish root");
                assert!(batch.page_count().expect("page count") > 3);
                let root = tree.root();
                assert_eq!(
                    batch
                        .commit()
                        .expect_err("injected multi-page fault must fail")
                        .code(),
                    ErrorCode::Io
                );
                root
            };

            let stop = nth_fault_index(&baseline, point, occurrence);
            assert_eq!(faults.observed(), baseline[..=stop]);
            assert!(pager.is_poisoned());
            assert_eq!(
                pager
                    .read(root)
                    .expect_err("unpublished root remains invisible")
                    .code(),
                ErrorCode::Corruption
            );
            let write_error = match pager.begin_write() {
                Ok(_) => panic!("poisoned pager must reject write batches"),
                Err(error) => error,
            };
            assert_eq!(write_error.code(), ErrorCode::Transaction);

            drop(pager);
            if let Ok(reopened) = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            ) {
                assert!(!reopened.is_poisoned());
            }
        }
    }
}

fn commit_multi_page_tree(faults: Arc<RecordingFaults>) -> Result<Vec<FaultPoint>> {
    let temp = TempDatabase::new("multi-page-fault-baseline");
    let pager =
        Pager::create(temp.path(), PagerConfig::test_default(4096))?.with_faults(faults.clone());
    let mut batch = pager.begin_write()?;
    let mut tree = BTree::create(&mut batch, TreeKind::Table)?;
    insert_multi_page_fixture(&mut tree, &mut batch)?;
    batch.track_tree(TreeKind::Table, tree.root())?;
    batch.set_catalog_root(tree.root())?;
    batch.commit()?;
    Ok(faults.observed())
}

fn insert_multi_page_fixture(
    tree: &mut BTree,
    batch: &mut super::pager::WriteBatch<'_>,
) -> Result<()> {
    for index in 0..30u8 {
        tree.insert(batch, &index.to_be_bytes(), &[index; 400])?;
    }
    Ok(())
}

fn nth_fault_index(events: &[FaultPoint], point: FaultPoint, occurrence: usize) -> usize {
    events
        .iter()
        .enumerate()
        .filter(|(_, seen)| **seen == point)
        .nth(occurrence - 1)
        .map(|(index, _)| index)
        .expect("recorded configured fault occurrence")
}

const MODEL_OPERATIONS: usize = 10_000;
const MODEL_VALIDATION_INTERVAL: usize = 100;
const MODEL_PERSISTENCE_INTERVAL: usize = 1_000;
const MODEL_SEED: u64 = 0x8d26_1a75_40ce_9b31;
const MODEL_DUPLICATE_INSERTS_PER_WINDOW: usize = 9;
const MODEL_MIN_DISTINCT_KEYS: usize = 3_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelProof {
    operations: usize,
    validations: usize,
    commits: usize,
    reopens: usize,
    distinct_keys: usize,
    tree_height: u64,
    duplicate_insert_attempts: usize,
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
#[ignore = "exhaustive six-configuration storage model; run in dedicated CI"]
fn fixed_seed_reference_model_covers_every_page_size_and_tree_kind() {
    let mut proofs = Vec::new();
    for page_size in [4096, 8192, 16384] {
        for kind in [TreeKind::Table, TreeKind::Index] {
            let proof = run_reference_model(page_size, kind).expect("fixed-seed reference model");
            proofs.push((page_size, kind, proof));
        }
    }

    for (page_size, kind, proof) in proofs {
        assert_eq!(proof.operations, MODEL_OPERATIONS);
        assert_eq!(proof.validations, MODEL_OPERATIONS / 100);
        assert_eq!(proof.commits, MODEL_OPERATIONS / MODEL_PERSISTENCE_INTERVAL);
        assert_eq!(proof.reopens, MODEL_OPERATIONS / MODEL_PERSISTENCE_INTERVAL);
        assert!(
            proof.distinct_keys >= MODEL_MIN_DISTINCT_KEYS,
            "page_size={page_size}, kind={kind:?} must exercise high-cardinality keys"
        );
        assert!(
            proof.tree_height >= 2,
            "page_size={page_size}, kind={kind:?} must form a nontrivial B+Tree shape"
        );
        assert_eq!(
            proof.duplicate_insert_attempts,
            (MODEL_OPERATIONS / MODEL_VALIDATION_INTERVAL) * MODEL_DUPLICATE_INSERTS_PER_WINDOW,
            "page_size={page_size}, kind={kind:?} must deliberately exercise duplicate inserts"
        );
    }
}

#[test]
fn reference_model_commits_once_per_persistence_window() {
    let proof = run_reference_model_operations(4096, TreeKind::Table, 1_000)
        .expect("reference-model persistence window");

    assert_eq!(proof.validations, 10);
    assert_eq!(proof.commits, 1, "1,000 operations must use one commit");
    assert_eq!(proof.reopens, 1);
}

fn run_reference_model(page_size: u32, kind: TreeKind) -> Result<ModelProof> {
    run_reference_model_operations(page_size, kind, MODEL_OPERATIONS)
}

fn run_reference_model_operations(
    page_size: u32,
    kind: TreeKind,
    operations: usize,
) -> Result<ModelProof> {
    assert_eq!(
        operations % MODEL_VALIDATION_INTERVAL,
        0,
        "model runs whole validation windows"
    );
    assert_eq!(
        operations % MODEL_PERSISTENCE_INTERVAL,
        0,
        "model commits whole persistence windows"
    );
    let temp = TempDatabase::new(&format!("model-{page_size}-{kind:?}"));
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    let mut tree = create_catalog_tree(&mut pager, kind)?;
    let mut model = BTreeMap::new();
    let mut random = XorShift64(MODEL_SEED);
    let mut fresh_key_sequence = 0u64;
    let mut query_key_sequence = 0u64;
    let mut seen_key_lengths = [false; 65];
    let mut validations = 0;
    let mut commits = 0;
    let mut reopens = 0;
    let mut duplicate_insert_attempts = 0;

    for persistence_window in 0..(operations / MODEL_PERSISTENCE_INTERVAL) {
        let mut batch = pager.begin_write()?;
        for checkpoint in 0..(MODEL_PERSISTENCE_INTERVAL / MODEL_VALIDATION_INTERVAL) {
            let window = persistence_window
                * (MODEL_PERSISTENCE_INTERVAL / MODEL_VALIDATION_INTERVAL)
                + checkpoint;
            for insert_index in 0..45 {
                let deliberate_duplicate = insert_index % 5 == 4;
                let key = if deliberate_duplicate {
                    known_model_key(&mut random, &model)
                } else {
                    fresh_workload_key(&mut random, &mut fresh_key_sequence, &model)
                };
                seen_key_lengths[key.len()] = true;
                let value = workload_value(page_size, &mut random);
                let expected = if deliberate_duplicate {
                    Err(ErrorCode::Constraint)
                } else {
                    Ok(())
                };
                let actual = tree
                    .insert(&mut batch, &key, &value)
                    .map_err(|error| error.code());
                assert_eq!(
                    actual, expected,
                    "insert result must mirror the reference model"
                );
                if deliberate_duplicate {
                    duplicate_insert_attempts += 1;
                } else {
                    model.insert(key, value);
                }
            }
            for delete_index in 0..25 {
                let key = query_workload_key(&mut random, &mut query_key_sequence, &model, 8);
                seen_key_lengths[key.len()] = true;
                let expected = model.remove(&key);
                let actual = tree.delete(&mut batch, &key).map_err(|error| {
                    DbError::new(
                        error.code(),
                        format!(
                            "reference-model delete failed at page_size={page_size}, kind={kind:?}, window={window}, delete={delete_index}: {error}"
                        ),
                    )
                })?;
                assert_eq!(
                    actual, expected,
                    "delete result must mirror the reference model"
                );
            }

            for _ in 0..20 {
                let key = query_workload_key(&mut random, &mut query_key_sequence, &model, 2);
                seen_key_lengths[key.len()] = true;
                assert_eq!(
                    tree.get(&mut batch, &key)?,
                    model.get(&key).cloned(),
                    "point lookup must mirror the reference model"
                );
            }
            for _ in 0..10 {
                let first = query_workload_key(&mut random, &mut query_key_sequence, &model, 2);
                let second = query_workload_key(&mut random, &mut query_key_sequence, &model, 2);
                seen_key_lengths[first.len()] = true;
                seen_key_lengths[second.len()] = true;
                let (start, end) = if first <= second {
                    (first, second)
                } else {
                    (second, first)
                };
                assert_eq!(
                    collect_range(&tree, &mut batch, &start, &end)?,
                    model_range(&model, &start, &end),
                    "range scan must mirror the reference model"
                );
            }

            validate_tree(&mut batch, &tree)?;
            validations += 1;
        }

        batch.track_tree(kind, tree.root())?;
        batch.set_catalog_root(tree.root())?;
        batch.commit()?;
        commits += 1;

        drop(pager);
        pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(page_size),
        )?;
        let mut snapshot = pager.begin_read()?;
        tree = snapshot
            .catalog_tree(kind)?
            .ok_or_else(|| DbError::new(ErrorCode::Corruption, "model catalog root is missing"))?;
        assert_eq!(
            collect_all(&tree, &mut snapshot)?,
            model
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>(),
            "full scan after reopen must mirror the reference model"
        );
        reopens += 1;
    }

    assert!(
        seen_key_lengths.into_iter().all(|seen| seen),
        "fixed-seed model must cover key lengths 0 through 64"
    );
    let mut snapshot = pager.begin_read()?;
    let tree_height = tree_height(&mut snapshot, tree.root())?;

    Ok(ModelProof {
        operations,
        validations,
        commits,
        reopens,
        distinct_keys: model.len(),
        tree_height,
        duplicate_insert_attempts,
    })
}

fn create_catalog_tree(pager: &mut Pager, kind: TreeKind) -> Result<BTree> {
    let mut batch = pager.begin_write()?;
    let tree = BTree::create(&mut batch, kind)?;
    batch.track_tree(kind, tree.root())?;
    batch.set_catalog_root(tree.root())?;
    batch.commit()?;
    Ok(tree)
}

fn fresh_workload_key(
    random: &mut XorShift64,
    next_sequence: &mut u64,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    loop {
        let sequence = next_workload_key_sequence(next_sequence);
        let key = sequenced_workload_key(random, sequence);
        if !model.contains_key(&key) {
            return key;
        }
    }
}

fn query_workload_key(
    random: &mut XorShift64,
    next_sequence: &mut u64,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    existing_divisor: u64,
) -> Vec<u8> {
    assert_ne!(
        existing_divisor, 0,
        "query key selection divisor must be nonzero"
    );
    if !model.is_empty() && random.next() % existing_divisor == 0 {
        known_model_key(random, model)
    } else {
        sequenced_workload_key(random, next_workload_key_sequence(next_sequence))
    }
}

fn known_model_key(random: &mut XorShift64, model: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let model_len = u64::try_from(model.len()).expect("model length fits u64");
    let index = usize::try_from(random.next() % model_len).expect("model index fits usize");
    model
        .keys()
        .nth(index)
        .expect("nonempty model must contain the selected key")
        .clone()
}

fn next_workload_key_sequence(next_sequence: &mut u64) -> u64 {
    let sequence = *next_sequence;
    *next_sequence = next_sequence
        .checked_add(1)
        .expect("fixed-seed key sequence must not overflow");
    sequence
}

fn sequenced_workload_key(random: &mut XorShift64, sequence: u64) -> Vec<u8> {
    let key_len = usize::try_from(sequence % 65).expect("model key length fits usize");
    let mut key = vec![0; key_len];
    let entropy = random.next().to_le_bytes();
    let sequence_bytes = sequence.to_le_bytes();
    for (index, byte) in key.iter_mut().enumerate() {
        let index_byte = u8::try_from(index).expect("key index fits u8");
        *byte = entropy[index % entropy.len()]
            ^ sequence_bytes[(index * 3) % sequence_bytes.len()]
                .wrapping_add(index_byte.wrapping_mul(17));
    }
    key
}

fn workload_value(page_size: u32, random: &mut XorShift64) -> Vec<u8> {
    let page_size = usize::try_from(page_size).expect("supported page size fits usize");
    let len = match random.next() % 10 {
        0 => 0,
        1 => 31,
        2..=4 => 127,
        5 | 6 => page_size / 2,
        7 | 8 => page_size.saturating_sub(128),
        _ => page_size + 111,
    };
    vec![random.next() as u8; len]
}

fn collect_all(tree: &BTree, source: &mut impl PageSource) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut cursor = tree.scan(source, Bound::Unbounded, Bound::Unbounded)?;
    let mut entries = Vec::new();
    while let Some(entry) = cursor.next()? {
        entries.push(entry);
    }
    Ok(entries)
}

fn collect_range(
    tree: &BTree,
    source: &mut impl PageSource,
    start: &[u8],
    end: &[u8],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut cursor = tree.scan(source, Bound::Included(start), Bound::Excluded(end))?;
    let mut entries = Vec::new();
    while let Some(entry) = cursor.next()? {
        entries.push(entry);
    }
    Ok(entries)
}

fn model_range(
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: &[u8],
    end: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    model
        .iter()
        .filter(|(key, _)| key.as_slice() >= start && key.as_slice() < end)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

struct CorruptionFixture {
    temp: TempDatabase,
    page_size: u32,
    table: BTree,
    index: BTree,
    pages: Vec<(PageRef, PageType)>,
}

#[test]
fn checksum_corruption_is_rejected_for_every_page_type_at_every_page_size() {
    for page_size in [4096, 8192, 16384] {
        let fixture = corruption_fixture(page_size).expect("create corruption fixture");
        for (reference, page_type) in &fixture.pages {
            flip_page_byte(
                fixture.temp.path(),
                fixture.page_size,
                *reference,
                page_size as usize - 1,
            )
            .expect("corrupt whole-page checksum");
            let error = match Pager::open(
                fixture.temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(page_size),
            ) {
                Ok(pager) => pager
                    .read(*reference)
                    .expect_err("checksum-corrupted page must reject reads"),
                Err(error) => error,
            };
            assert_eq!(
                error.code(),
                ErrorCode::Corruption,
                "checksum corruption must reject {page_type:?} at page size {page_size}"
            );
        }
    }
}

#[test]
fn truncation_and_restamped_field_corruption_return_controlled_errors() {
    for truncate_to in [127u64, 4097, 4096] {
        let fixture = corruption_fixture(4096).expect("create truncation fixture");
        truncate_file(fixture.temp.path(), truncate_to).expect("truncate fixture");
        assert_controlled_corruption(|| {
            Pager::open(
                fixture.temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            )
            .map(|_| ())
        });
    }

    let fixture = corruption_fixture(4096).expect("create header corruption fixture");
    flip_header_byte(fixture.temp.path(), SuperblockSlot::A, 0).expect("corrupt first superblock");
    Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(4096),
    )
    .expect("one valid superblock remains usable");
    flip_header_byte(fixture.temp.path(), SuperblockSlot::B, 0).expect("corrupt second superblock");
    assert_controlled_corruption(|| {
        Pager::open(
            fixture.temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .map(|_| ())
    });

    let fixture = corruption_fixture(4096).expect("create page id fixture");
    let leaf = fixture.page(PageType::TableLeaf);
    patch_page(&fixture, leaf, 4, &u64::MAX.to_le_bytes()).expect("restamp page id");
    assert_controlled_corruption(|| read_page_from_fixture(&fixture, leaf));

    let fixture = corruption_fixture(4096).expect("create generation fixture");
    let leaf = fixture.page(PageType::TableLeaf);
    patch_page(&fixture, leaf, 12, &0u64.to_le_bytes()).expect("restamp generation");
    assert_controlled_corruption(|| read_page_from_fixture(&fixture, leaf));

    let fixture = corruption_fixture(4096).expect("create slot fixture");
    let leaf = fixture.page(PageType::TableLeaf);
    patch_page(&fixture, leaf, 56, &[0xfe, 0x0f, 0x04, 0x00]).expect("restamp slot");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Table));

    let fixture = corruption_fixture(4096).expect("create sibling fixture");
    let sibling_leaf = fixture
        .pages
        .iter()
        .find_map(|(reference, page_type)| {
            (*page_type == PageType::TableLeaf
                && page_bytes(&fixture, *reference)[28..36] != [0; 8])
                .then_some(*reference)
        })
        .expect("fixture has a linked table leaf");
    patch_page(&fixture, sibling_leaf, 28, &[0; 8]).expect("restamp sibling");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Table));

    let fixture = corruption_fixture(4096).expect("create child fixture");
    let root = fixture.table.root();
    patch_page(&fixture, root, 48, &[0; 8]).expect("restamp child reference");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Table));

    let fixture = corruption_fixture(4096).expect("create overflow fixture");
    let overflow = fixture
        .pages
        .iter()
        .find_map(|(reference, page_type)| {
            (*page_type == PageType::Overflow && page_bytes(&fixture, *reference)[48..56] != [0; 8])
                .then_some(*reference)
        })
        .expect("fixture has a linked overflow page");
    patch_page(&fixture, overflow, 48, &[0; 8]).expect("restamp overflow link");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Index));

    let fixture = corruption_fixture(4096).expect("create freelist fixture");
    let freelist = fixture.page(PageType::Freelist);
    patch_page(&fixture, freelist, 68, &1u32.to_le_bytes())
        .expect("restamp freelist reserved field");
    assert_controlled_corruption(|| {
        let pager = Pager::open(
            fixture.temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )?;
        pager.begin_write().map(|_| ())
    });
}

impl CorruptionFixture {
    fn page(&self, page_type: PageType) -> PageRef {
        self.pages
            .iter()
            .find_map(|(reference, seen_type)| (*seen_type == page_type).then_some(*reference))
            .unwrap_or_else(|| panic!("fixture is missing {page_type:?}"))
    }
}

fn corruption_fixture(page_size: u32) -> Result<CorruptionFixture> {
    let temp = TempDatabase::new(&format!("corruption-{page_size}"));
    let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    let (mut table, index) = {
        let mut batch = pager.begin_write()?;
        let mut table = BTree::create(&mut batch, TreeKind::Table)?;
        let mut index = BTree::create(&mut batch, TreeKind::Index)?;
        for number in 0..24u8 {
            table.insert(&mut batch, &fixture_key(0x11, number), b"table")?;
            index.insert(&mut batch, &fixture_key(0x22, number), b"index")?;
        }
        table.insert(
            &mut batch,
            b"released-overflow",
            &vec![0x31; usize::try_from(page_size).expect("page size fits usize") + 111],
        )?;
        index.insert(
            &mut batch,
            b"live-overflow",
            &vec![0x32; usize::try_from(page_size).expect("page size fits usize") + 111],
        )?;
        batch.allocate_append(PageType::Catalog)?;
        batch.track_tree(TreeKind::Table, table.root())?;
        batch.track_tree(TreeKind::Index, index.root())?;
        batch.set_catalog_root(table.root())?;
        batch.commit()?;
        (table, index)
    };
    {
        let mut batch = pager.begin_write()?;
        assert!(table.delete(&mut batch, b"released-overflow")?.is_some());
        batch.track_tree(TreeKind::Table, table.root())?;
        batch.track_tree(TreeKind::Index, index.root())?;
        batch.set_catalog_root(table.root())?;
        batch.commit()?;
    }
    let pages = materialize_wal_fixture_to_checkpoint(&temp, pager, table.root().page_id())?;
    for page_type in [
        PageType::Catalog,
        PageType::TableInternal,
        PageType::TableLeaf,
        PageType::IndexInternal,
        PageType::IndexLeaf,
        PageType::Overflow,
        PageType::Freelist,
    ] {
        assert!(
            pages.iter().any(|(_, seen_type)| *seen_type == page_type),
            "fixture must contain {page_type:?}"
        );
    }
    Ok(CorruptionFixture {
        temp,
        page_size,
        table,
        index,
        pages,
    })
}

fn materialize_wal_fixture_to_checkpoint(
    temp: &TempDatabase,
    pager: Pager,
    catalog_root: PageId,
) -> Result<Vec<(PageRef, PageType)>> {
    let latest_lsn = pager.latest_lsn()?;
    let mut snapshot = pager.begin_read()?;
    let page_count = snapshot.page_count()?;
    let freelist_root = snapshot.freelist_root()?;
    let mut images = Vec::new();
    let mut pages = Vec::new();
    for page_id in 1..page_count {
        let page_id = PageId(page_id);
        let frame = snapshot.read_page_by_id(page_id)?;
        let reference = PageRef::new(frame.header().page_id(), frame.header().generation())?;
        images.push((page_id, frame.bytes().to_vec()));
        pages.push((reference, frame.header().page_type()));
    }
    drop(snapshot);
    drop(pager);

    let mut main =
        DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)?;
    main.extend_to_pages(page_count)?;
    for (page_id, bytes) in images {
        main.write_page(page_id, &bytes)?;
    }
    main.sync_all()?;
    let base = main.header();
    let checkpoint = base.with_checkpoint(
        latest_lsn,
        page_count,
        catalog_root,
        freelist_root,
        base.wal_generation(),
    )?;
    main.publish_superblock(checkpoint)?;
    let fallback_checkpoint = main.header().with_checkpoint(
        latest_lsn,
        page_count,
        catalog_root,
        freelist_root,
        main.header().wal_generation(),
    )?;
    main.publish_superblock(fallback_checkpoint)?;
    drop(main);

    fs::remove_file(wal_path(temp.path())).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("remove fixture WAL before rebasing: {error}"),
        )
    })?;
    let wal = Wal::create(temp.path(), &fallback_checkpoint)?;
    drop(wal);
    Ok(pages)
}

fn fixture_key(prefix: u8, number: u8) -> Vec<u8> {
    let mut key = vec![prefix; 1024];
    key[..2].copy_from_slice(&[prefix, number]);
    key
}

fn fixture_pages(path: &Path, page_size: u32) -> Result<Vec<(PageRef, PageType)>> {
    let bytes = fs::read(path).map_err(|error| {
        DbError::new(ErrorCode::Io, format!("read corruption fixture: {error}"))
    })?;
    let page_len = usize::try_from(page_size).expect("page size fits usize");
    let ordinary_bytes = bytes
        .len()
        .checked_sub(usize::try_from(SUPERBLOCK_AREA_LEN).expect("prefix length fits usize"))
        .ok_or_else(|| DbError::new(ErrorCode::Corruption, "fixture omits superblocks"))?;
    let mut pages = Vec::new();
    for page_id in 1..=ordinary_bytes / page_len {
        let page_id = PageId(u64::try_from(page_id).expect("page id fits u64"));
        let offset = fixture_page_offset(page_id, page_size)?;
        let frame = PageFrame::decode(
            bytes[offset..offset + page_len].to_vec(),
            page_id,
            None,
            None,
        )?;
        let reference = PageRef::new(frame.header().page_id(), frame.header().generation())?;
        pages.push((reference, frame.header().page_type()));
    }
    Ok(pages)
}

fn page_bytes(fixture: &CorruptionFixture, reference: PageRef) -> Vec<u8> {
    let bytes = fs::read(fixture.temp.path()).expect("read fixture bytes");
    let page_len = usize::try_from(fixture.page_size).expect("page size fits usize");
    let offset =
        fixture_page_offset(reference.page_id(), fixture.page_size).expect("fixture page offset");
    bytes[offset..offset + page_len].to_vec()
}

fn patch_page(
    fixture: &CorruptionFixture,
    reference: PageRef,
    offset: usize,
    replacement: &[u8],
) -> Result<()> {
    let mut bytes = fs::read(fixture.temp.path())
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read fixture for patch: {error}")))?;
    let page_len = usize::try_from(fixture.page_size).expect("page size fits usize");
    let page_offset = fixture_page_offset(reference.page_id(), fixture.page_size)?;
    let page = &mut bytes[page_offset..page_offset + page_len];
    page[offset..offset + replacement.len()].copy_from_slice(replacement);
    stamp_page_checksum(page)?;
    fs::write(fixture.temp.path(), bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write patched fixture: {error}")))
}

fn flip_page_byte(path: &Path, page_size: u32, reference: PageRef, offset: usize) -> Result<()> {
    let mut bytes = fs::read(path).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("read fixture for checksum flip: {error}"),
        )
    })?;
    let page_offset = fixture_page_offset(reference.page_id(), page_size)?;
    bytes[page_offset + offset] ^= 1;
    fs::write(path, bytes).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("write checksum-flipped fixture: {error}"),
        )
    })
}

fn flip_header_byte(path: &Path, slot: SuperblockSlot, offset: usize) -> Result<()> {
    if offset >= SUPERBLOCK_SLOT_LEN {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "superblock corruption offset is outside the selected slot",
        ));
    }
    let mut bytes = fs::read(path)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read header fixture: {error}")))?;
    let slot_start = match slot {
        SuperblockSlot::A => 0,
        SuperblockSlot::B => SUPERBLOCK_SLOT_LEN,
    };
    bytes[slot_start + offset] ^= 1;
    fs::write(path, bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write header fixture: {error}")))
}

fn fixture_page_offset(page_id: PageId, page_size: u32) -> Result<usize> {
    usize::try_from(checked_page_offset(page_id, page_size, u64::MAX)?)
        .map_err(|_| DbError::new(ErrorCode::Corruption, "fixture page offset exceeds usize"))
}

fn truncate_file(path: &Path, len: u64) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("open truncation fixture: {error}")))?
        .set_len(len)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("truncate fixture: {error}")))
}

fn read_page_from_fixture(fixture: &CorruptionFixture, reference: PageRef) -> Result<()> {
    let pager = Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(fixture.page_size),
    )?;
    pager.read(reference).map(|_| ())
}

fn validate_fixture_tree(fixture: &CorruptionFixture, kind: TreeKind) -> Result<()> {
    let pager = Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(fixture.page_size),
    )?;
    let tree = match kind {
        TreeKind::Table => &fixture.table,
        TreeKind::Index => &fixture.index,
    };
    let mut snapshot = pager.begin_read()?;
    validate_tree(&mut snapshot, tree).map(|_| ())
}

fn assert_controlled_corruption(action: impl FnOnce() -> Result<()>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .expect("corruption path must not panic");
    assert_eq!(
        result
            .expect_err("corruption path must return an error")
            .code(),
        ErrorCode::Corruption
    );
}

struct ScaleMetrics {
    rows: u64,
    file_bytes: u64,
    tree_height: u64,
    reused_pages: u64,
    elapsed: Duration,
}

#[test]
#[ignore = "manual million-record Phase 2 scale probe"]
fn million_small_records_report_storage_shape() {
    let metrics = run_scale_probe(1_000_000, 8192).expect("scale probe");
    eprintln!(
        "rows={} bytes={} height={} reused_pages={} elapsed_ms={}",
        metrics.rows,
        metrics.file_bytes,
        metrics.tree_height,
        metrics.reused_pages,
        metrics.elapsed.as_millis()
    );
    assert_eq!(metrics.rows, 1_000_000);
}

fn run_scale_probe(rows: u64, page_size: u32) -> Result<ScaleMetrics> {
    const BATCH_ROWS: u64 = 10_000;

    let started = Instant::now();
    let temp = TempDatabase::new("million-small-records");
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    let mut tree = create_catalog_tree(&mut pager, TreeKind::Table)?;
    let mut next_row = 0u64;

    while next_row < rows {
        let mut batch = pager.begin_write()?;
        let end = next_row.saturating_add(BATCH_ROWS).min(rows);
        while next_row < end {
            tree.insert(&mut batch, &next_row.to_be_bytes(), b"phase-2-scale")?;
            next_row += 1;
        }
        batch.track_tree(TreeKind::Table, tree.root())?;
        batch.set_catalog_root(tree.root())?;
        batch.commit()?;
    }
    let file_bytes = SUPERBLOCK_AREA_LEN
        .checked_add(
            pager
                .page_count()
                .checked_sub(1)
                .and_then(|ordinary_pages| ordinary_pages.checked_mul(u64::from(page_size)))
                .ok_or_else(|| DbError::new(ErrorCode::Corruption, "scale file length overflow"))?,
        )
        .ok_or_else(|| DbError::new(ErrorCode::Corruption, "scale file length overflow"))?;
    let mut snapshot = pager.begin_read()?;
    let tree_height = tree_height(&mut snapshot, tree.root())?;

    Ok(ScaleMetrics {
        rows,
        file_bytes,
        tree_height,
        reused_pages: pager.reused_page_count(),
        elapsed: started.elapsed(),
    })
}

fn tree_height(source: &mut impl PageSource, mut current: PageRef) -> Result<u64> {
    let mut height = 0u64;
    loop {
        let frame = source.read_page(current)?;
        height = height
            .checked_add(1)
            .ok_or_else(|| DbError::new(ErrorCode::Corruption, "scale tree height overflow"))?;
        current = match frame.header().page_type() {
            PageType::TableLeaf | PageType::IndexLeaf => return Ok(height),
            PageType::TableInternal | PageType::IndexInternal => {
                super::btree::decode_internal_leftmost(&frame.bytes()[48..64])?
            }
            _ => {
                return Err(DbError::new(
                    ErrorCode::Corruption,
                    "scale tree contains a non-B+Tree page",
                ));
            }
        };
    }
}
