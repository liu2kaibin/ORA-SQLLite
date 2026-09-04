use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
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
    PageId, PageRef,
    btree::{BTree, TreeKind},
    checksum::stamp_page_checksum,
    file::OpenMode,
    page::{PageFrame, PageType},
    pager::{PageSource, Pager, PagerConfig},
    validate::validate_tree,
};

static NEXT_TEMP_DATABASE: AtomicU64 = AtomicU64::new(0);

pub(super) struct TempDatabase {
    path: PathBuf,
}

impl TempDatabase {
    pub(super) fn new(label: &str) -> Self {
        let process_id = std::process::id();
        let label = sanitized_label(label);

        loop {
            let sequence = NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed);
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
        if let Err(error) = fs::remove_file(&self.path) {
            if error.kind() != ErrorKind::NotFound {
                // Test cleanup is best effort; a later test must never remove a different path.
            }
        }
    }
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
    let sequence = NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed);
    let first_sequence = sequence
        .checked_add(1)
        .expect("temporary database sequence must not overflow");
    let collision = std::env::temp_dir().join(format!(
        "ora-sqllite-{}-{label}-{first_sequence}.db",
        std::process::id(),
    ));
    let first_candidate = std::env::temp_dir().join(format!(
        "ora-sqllite-{}-{label}-{first_sequence}.db",
        std::process::id(),
    ));
    assert_eq!(
        collision, first_candidate,
        "the collision fixture must occupy TempDatabase::new's first candidate"
    );
    fs::write(&collision, b"pre-existing collision").expect("create collision file");

    let database = TempDatabase::new(label);
    let database_path = database.path().to_path_buf();
    let retry_sequence = first_sequence
        .checked_add(1)
        .expect("temporary database retry sequence must not overflow");
    assert_eq!(
        database_path,
        std::env::temp_dir().join(format!(
            "ora-sqllite-{}-{label}-{retry_sequence}.db",
            std::process::id()
        )),
        "TempDatabase::new must retry immediately after the occupied first candidate"
    );
    let sentinel = database_path.with_extension("sentinel");
    assert_ne!(database_path, collision);
    fs::write(&database_path, b"owned database file").expect("create owned file");
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
        sentinel.exists(),
        "Drop must not remove an adjacent unrelated filename"
    );
    fs::remove_file(collision).expect("remove collision fixture");
    fs::remove_file(sentinel).expect("remove sentinel fixture");
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
    let faults = Arc::new(RecordingFaults::at(FaultPoint::BeforeFileWrite, 1));
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
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

    assert_eq!(faults.observed(), vec![FaultPoint::BeforeFileWrite]);
    assert!(pager.is_poisoned());
    assert_eq!(
        pager.read(page).expect_err("poisoned read").code(),
        ErrorCode::Io
    );
}

#[test]
fn every_multi_page_write_and_sync_fault_poisons_the_pager_until_reopen() {
    let baseline_faults = Arc::new(RecordingFaults::never());
    let baseline = commit_multi_page_tree(baseline_faults.clone()).expect("baseline commit");
    assert!(
        baseline
            .iter()
            .filter(|&&point| point == FaultPoint::BeforeFileWrite)
            .count()
            > 2,
        "fixture must make a multi-page commit"
    );

    for point in [
        FaultPoint::BeforeFileWrite,
        FaultPoint::AfterFileWrite,
        FaultPoint::BeforeFileSync,
        FaultPoint::AfterFileSync,
    ] {
        let occurrences = baseline.iter().filter(|&&seen| seen == point).count();
        for occurrence in 1..=occurrences {
            let faults = Arc::new(RecordingFaults::at(point, occurrence));
            let temp = TempDatabase::new(&format!("multi-page-fault-{point:?}-{occurrence}"));
            let mut pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
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
                pager.read(root).expect_err("poisoned pager read").code(),
                ErrorCode::Io
            );
            let write_error = match pager.begin_write() {
                Ok(_) => panic!("poisoned pager must reject write batches"),
                Err(error) => error,
            };
            assert_eq!(write_error.code(), ErrorCode::Io);

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
    let mut pager =
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
        tree = pager
            .catalog_tree(kind)?
            .ok_or_else(|| DbError::new(ErrorCode::Corruption, "model catalog root is missing"))?;
        assert_eq!(
            collect_all(&tree, &mut pager)?,
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
    let tree_height = tree_height(temp.path(), page_size, tree.root())?;

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
            let mut pager = Pager::open(
                fixture.temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(page_size),
            )
            .expect("open checksum-corrupted fixture");
            assert_eq!(
                pager
                    .read(*reference)
                    .expect_err("checksum-corrupted page must reject reads")
                    .code(),
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
    flip_header_byte(fixture.temp.path(), 0).expect("corrupt header");
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
        let mut pager = Pager::open(
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
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
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
    drop(pager);

    let pages = fixture_pages(temp.path(), page_size)?;
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
    let mut pages = Vec::new();
    for page_id in 1..bytes.len() / page_len {
        let offset = page_id * page_len;
        let frame = PageFrame::decode(
            bytes[offset..offset + page_len].to_vec(),
            PageId(u64::try_from(page_id).expect("page id fits u64")),
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
    let offset = usize::try_from(reference.page_id().0).expect("page id fits usize") * page_len;
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
    let page_offset =
        usize::try_from(reference.page_id().0).expect("page id fits usize") * page_len;
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
    let page_len = usize::try_from(page_size).expect("page size fits usize");
    let page_offset =
        usize::try_from(reference.page_id().0).expect("page id fits usize") * page_len;
    bytes[page_offset + offset] ^= 1;
    fs::write(path, bytes).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("write checksum-flipped fixture: {error}"),
        )
    })
}

fn flip_header_byte(path: &Path, offset: usize) -> Result<()> {
    let mut bytes = fs::read(path)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read header fixture: {error}")))?;
    bytes[offset] ^= 1;
    fs::write(path, bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write header fixture: {error}")))
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
    let mut pager = Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(fixture.page_size),
    )?;
    pager.read(reference).map(|_| ())
}

fn validate_fixture_tree(fixture: &CorruptionFixture, kind: TreeKind) -> Result<()> {
    let mut pager = Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(fixture.page_size),
    )?;
    let tree = match kind {
        TreeKind::Table => &fixture.table,
        TreeKind::Index => &fixture.index,
    };
    validate_tree(&mut pager, tree).map(|_| ())
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
    let file_bytes = fs::metadata(temp.path())
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read scale file metadata: {error}")))?
        .len();
    let tree_height = tree_height(temp.path(), page_size, tree.root())?;

    Ok(ScaleMetrics {
        rows,
        file_bytes,
        tree_height,
        reused_pages: pager.reused_page_count(),
        elapsed: started.elapsed(),
    })
}

fn tree_height(path: &Path, page_size: u32, mut current: PageRef) -> Result<u64> {
    let bytes = fs::read(path)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read scale tree: {error}")))?;
    let page_len = usize::try_from(page_size).expect("page size fits usize");
    let mut height = 0u64;
    loop {
        let offset = usize::try_from(current.page_id().0).expect("page id fits usize") * page_len;
        let frame = PageFrame::decode(
            bytes[offset..offset + page_len].to_vec(),
            current.page_id(),
            None,
            Some(current.generation()),
        )?;
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
