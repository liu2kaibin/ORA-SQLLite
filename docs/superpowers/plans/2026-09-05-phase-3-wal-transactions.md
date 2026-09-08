# Phase 3 WAL, Transactions, and Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a crash-safe full-page WAL, deterministic recovery, transaction-level read snapshots, single-writer concurrency, checkpointing, and cross-process locking to the Phase 2 storage engine.

**Architecture:** `WriteBatch` retains private page images and publishes them only after page-frame records plus one commit record are synced to a sidecar WAL. `ReadSnapshot` resolves the newest WAL version not newer than its captured LSN and otherwise reads the checkpointed main file. A serialized checkpoint writes a safe snapshot into the main file, publishes one of two superblocks, and resets the WAL generation only when no reader can need an old version.

**Tech Stack:** Rust 2024, MSRV Rust 1.85, standard-library synchronization and file I/O, CRC32C already implemented in the crate, `fs4 = 1.1.0` with only its synchronous feature.

**Spec:** `docs/superpowers/specs/2026-09-05-phase-3-wal-transactions-design.md`

## Global Constraints

- Keep `#![forbid(unsafe_code)]`; dependency internals do not relax this rule for project code.
- Keep every Phase 3 storage API crate-private; do not add SQL, catalog schema, public CRUD APIs, nested transactions, savepoints, async I/O, mmap, or background threads.
- Support only full durability: a commit succeeds only after the complete WAL transaction has passed `sync_all`.
- Use disk-format major version 2 with two 4096-byte main-file superblock slots and an 8192-byte fixed prefix.
- Use WAL-format version 1 with two 4096-byte header slots and records beginning at offset 8192.
- Keep all integer encoding little-endian and all length, offset, count, generation, and LSN arithmetic checked.
- Keep database page sizes limited to 4096, 8192, and 16384 bytes.
- Keep the database size target at 10 GiB, automatic checkpoint threshold at 64 MiB, and WAL hard limit at 1 GiB.
- Keep one process per database file, multiple transaction-level read snapshots, and one writer/checkpointer.
- Treat Phase 2 format-1 files as `Unsupported`; do not add a migration path.
- Pin `fs4` exactly to `1.1.0`, disable default features, and enable only `sync`; do not commit `Cargo.lock` for this library crate.
- Every task follows red-green-refactor TDD and ends in one reviewable commit.

---

### Task 1: Replace runtime-borrowed page handles with immutable `Arc` handles

**Files:**
- Modify: `src/storage/page.rs`
- Modify: `src/storage/cache.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/btree/mod.rs`
- Modify: `src/storage/btree/search.rs`
- Modify: `src/storage/btree/mutate.rs`
- Modify: `src/storage/overflow.rs`
- Modify: `src/storage/validate.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Produces: `pub(super) type PageHandle = Arc<PageFrame>`.
- Preserves: `PageSource::read_page(&mut self, PageRef) -> Result<PageHandle>` for this task.
- Preserves: mutable pages exist only inside `WriteBatch::dirty: BTreeMap<PageId, PageFrame>`.

- [ ] **Step 1: Add an immutable-handle compile test and update cache expectations**

Add to `src/storage/cache.rs`:

```rust
#[test]
fn page_handles_are_shareable_immutable_arcs() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PageHandle>();

    let page = handle(1);
    let clone = page.clone();
    assert!(Arc::ptr_eq(&page, &clone));
    assert_eq!(page.header().page_id(), PageId(1));
}
```

Replace the obsolete mutable-borrow regression in `src/storage/pager.rs` with a test named `held_immutable_handle_does_not_block_reads_or_writes`. It must keep one `PageHandle`, commit a replacement image, and assert the held handle still contains the old byte while a new read contains the new byte.

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test storage::cache::page_handles_are_shareable_immutable_arcs -- --exact`

Expected: compilation fails because `PageHandle` is still `Rc<RefCell<PageFrame>>` and is not `Send + Sync`.

- [ ] **Step 3: Convert the handle and cache ownership model**

Do not derive `Clone` for `PageFrame`. Retain the existing byte re-decode in `copy_clean_frame` and `private_frame`, so every mutable private copy passes the same checksum/header validation boundary.

In `src/storage/cache.rs`, replace the alias and ownership checks:

```rust
use std::{collections::BTreeMap, sync::Arc};

pub(super) type PageHandle = Arc<PageFrame>;

// insert_clean
let page_id = handle.header().page_id();

// eviction filter
.filter(|(_, entry)| Arc::strong_count(&entry.handle) == 1)
```

In `pager.rs`, construct `Arc::new(frame)`, use `Arc::ptr_eq` in tests, and replace `handle.borrow()`/`try_borrow()` with immutable dereference. Use this exact private-handle boundary:

```rust
fn private_handle(frame: &mut PageFrame) -> Result<PageHandle> {
    Ok(Arc::new(private_frame(frame)?))
}
```

Apply these exact replacements in B+Tree, overflow, validation, and local fake `PageSource` implementations: `handle.borrow().header()` becomes `handle.header()`, `handle.borrow().bytes()` becomes `handle.bytes()`, `let frame = handle.borrow()` becomes `let frame = handle.as_ref()`, and `Rc::new(RefCell::new(frame))` becomes `Arc::new(frame)`. Delete code paths that mapped `try_borrow` failure to `Busy`; immutable handles cannot produce that state.

- [ ] **Step 4: Run all fast tests**

Run: `cargo test --all-targets --all-features`

Expected: 187 passed, 0 failed, 2 ignored. No source occurrence of `Rc`, `RefCell`, `.borrow()`, or `try_borrow()` remains under `src/storage`.

- [ ] **Step 5: Run formatting and lint checks**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: both pass.

- [ ] **Step 6: Commit**

```bash
git add src/storage
git commit -m "refactor(storage): use immutable shared page handles"
```

---

### Task 2: Introduce format-2 dual superblocks and the fixed main-file prefix

**Files:**
- Modify: `src/storage/header.rs`
- Modify: `src/storage/file.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Produces: `SUPERBLOCK_SLOT_LEN`, `SUPERBLOCK_AREA_LEN`, `SuperblockSlot`, and `DatabaseHeader::{encode_slot, decode_slot, select}`.
- Produces: `DatabaseHeader::{publish_sequence, checkpoint_lsn, page_count, wal_generation, with_checkpoint}`.
- Changes: ordinary-page offset to `8192 + (page_id - 1) * page_size`.
- Preserves: `PageId(0)` is invalid for ordinary page I/O; the first ordinary page remains `PageId(1)`.

- [ ] **Step 1: Replace the old header layout tests with format-2 golden tests**

Add these tests using module-private field access:

```rust
#[test]
fn superblock_slot_uses_fixed_format_2_offsets_and_crc() {
    let header = DatabaseHeader::new(4096, [0x5a; 16]).unwrap();
    let bytes = header.encode_slot();
    assert_eq!(bytes.len(), 4096);
    assert_eq!(&bytes[..16], b"ORA-SQLLITE-SB\0\0");
    assert_eq!(&bytes[16..20], &[2, 0, 0, 0]);
    assert_eq!(&bytes[20..24], &4096u32.to_le_bytes());
    assert_eq!(&bytes[24..40], &[0x5a; 16]);
    assert!(bytes[96..4092].iter().all(|byte| *byte == 0));
    assert_eq!(u32::from_le_bytes(bytes[4092..].try_into().unwrap()), crc32c(&bytes[..4092]));
}

#[test]
fn select_uses_the_highest_valid_publish_sequence() {
    let mut a = DatabaseHeader::new(4096, [1; 16]).unwrap();
    let mut b = a;
    a.publish_sequence = 7;
    b.publish_sequence = 8;
    let (selected, slot) = DatabaseHeader::select(&a.encode_slot(), &b.encode_slot()).unwrap();
    assert_eq!(selected.publish_sequence(), 8);
    assert_eq!(slot, SuperblockSlot::B);
}

#[test]
fn equal_sequences_with_different_state_are_corruption() {
    let mut a = DatabaseHeader::new(4096, [1; 16]).unwrap();
    let mut b = a;
    a.publish_sequence = 9;
    b.publish_sequence = 9;
    b.page_count = 2;
    b.catalog_root = PageId(1);
    assert_eq!(DatabaseHeader::select(&a.encode_slot(), &b.encode_slot()).unwrap_err().code(), ErrorCode::Corruption);
}
```

Add a file test asserting a new empty database is exactly 8192 bytes and both slots decode.

- [ ] **Step 2: Run the new header tests and verify they fail**

Run: `cargo test storage::header::superblock_slot_uses_fixed_format_2_offsets_and_crc -- --exact`

Expected: failure because the current encoder returns 128 bytes and format major 1.

- [ ] **Step 3: Implement the exact format-2 header model**

Use these declarations in `header.rs`:

```rust
pub(super) const SUPERBLOCK_SLOT_LEN: usize = 4096;
pub(super) const SUPERBLOCK_AREA_LEN: u64 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SuperblockSlot { A, B }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DatabaseHeader {
    major: u16,
    minor: u16,
    page_size: u32,
    uuid: [u8; 16],
    publish_sequence: u64,
    checkpoint_lsn: Lsn,
    page_count: u64,
    catalog_root: PageId,
    freelist_root: PageId,
    wal_generation: u64,
    features: u64,
}
```

Implement the offsets and selection rules from spec sections 4.1 and 4.2. `DatabaseHeader::new` starts with sequence 1, checkpoint 0, page count 1, WAL generation 1, and empty roots. `with_checkpoint` returns a validated copy and does not mutate the current active header in place.

- [ ] **Step 4: Implement fixed-prefix file I/O and alternating publication**

Change `checked_page_offset` to:

```rust
let ordinary_index = page_id.0.checked_sub(1)
    .ok_or_else(|| corruption("ordinary page I/O cannot access page zero"))?;
let offset = SUPERBLOCK_AREA_LEN
    .checked_add(ordinary_index.checked_mul(u64::from(page_size))
        .ok_or_else(|| corruption("page offset overflow"))?)
    .ok_or_else(|| corruption("page offset overflow"))?;
```

Store the active `SuperblockSlot` in `DatabaseFile`. Creation writes and syncs Slot A, then writes Slot B with sequence 2 and syncs again. Opening reads both 4096-byte slots, recognizes the old format-1 magic before generic corruption handling, and selects the valid newest slot. Replace `write_header` with:

```rust
pub(super) fn publish_superblock(&mut self, candidate: DatabaseHeader) -> Result<()>;
```

It writes only the inactive slot, syncs the main file, and changes the active slot/header only after sync succeeds.

- [ ] **Step 5: Update fixtures and corruption helpers for the new prefix**

Change every test helper that computes `page_id * page_size` to call the production checked offset or use `SUPERBLOCK_AREA_LEN + (page_id - 1) * page_size`. Make `flip_header_byte` accept `SuperblockSlot` and corrupt only that slot. Add coverage proving one corrupt slot still opens and two corrupt slots fail.

- [ ] **Step 6: Run the storage suite**

Run: `cargo test storage -- --nocapture`

Expected: all non-ignored storage tests pass; normal-close persistence works with the new layout.

- [ ] **Step 7: Commit**

```bash
git add src/storage/header.rs src/storage/file.rs src/storage/pager.rs src/storage/test_support.rs
git commit -m "feat(storage): add dual format 2 superblocks"
```

---

### Task 3: Add streaming CRC32C and exact WAL record codecs

**Files:**
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/checksum.rs`
- Create: `src/storage/wal/mod.rs`
- Create: `src/storage/wal/format.rs`

**Interfaces:**
- Produces: `Crc32c::{new, update, finish}`.
- Produces: `WalHeader`, `RecordHeader`, `CommitState`, `CommitPayload`, `RecordType`.
- Produces: exact 4096-byte WAL Header Slot, 64-byte Record Header, and 64-byte Commit Payload codecs.

- [ ] **Step 1: Write byte-golden and rejection tests**

In `wal/format.rs`, add tests for all three fixed layouts. The WAL-header test must assert magic, every numeric offset, all-zero reserved bytes, and CRC at `4092..4096`. The record test must exercise PageFrame and Commit types, 8-byte padding, payload CRC, header CRC with bytes `52..56` zeroed, and generation at `56..64`.

Add rejection tests for unknown record type, wrong fixed lengths, nonzero flags/reserved/padding, page zero, generation zero, payload-length mismatch, UUID mismatch, and checked record-end overflow.

- [ ] **Step 2: Run the codec tests and verify they fail**

Run: `cargo test storage::wal::format::tests -- --nocapture`

Expected: compilation fails because `storage::wal` and its types do not exist.

- [ ] **Step 3: Add an incremental checksum type**

Refactor the existing private updater into:

```rust
pub(super) struct Crc32c(u32);

impl Crc32c {
    pub(super) const fn new() -> Self { Self(!0) }
    pub(super) fn update(&mut self, bytes: &[u8]) { crc32c_update(&mut self.0, bytes); }
    pub(super) const fn finish(self) -> u32 { !self.0 }
}
```

Keep `crc32c(bytes)` implemented through this type and preserve existing vectors.

- [ ] **Step 4: Implement the WAL data types and fixed codecs**

Use the exact field types:

```rust
pub(super) struct WalHeader {
    pub(super) page_size: u32,
    pub(super) database_uuid: [u8; 16],
    pub(super) wal_generation: u64,
    pub(super) base_checkpoint_lsn: Lsn,
    pub(super) salt: [u8; 16],
    pub(super) header_sequence: u64,
}

pub(super) struct CommitState {
    pub(super) page_count: u64,
    pub(super) catalog_root: PageId,
    pub(super) freelist_root: PageId,
    pub(super) feature_bits: u64,
}

pub(super) struct RecordHeader {
    pub(super) record_type: RecordType,
    pub(super) payload_len: u32,
    pub(super) transaction_lsn: Lsn,
    pub(super) ordinal: u32,
    pub(super) page_id: PageId,
    pub(super) generation: u64,
    pub(super) payload_crc32c: u32,
    pub(super) wal_generation: u64,
}
```

Implement only bounded stack/fixed-vector allocation after validating lengths. Add `record_len(payload_len) -> Result<u64>` and `zero_padding_len(payload_len) -> usize` so file and recovery code share one arithmetic contract.

- [ ] **Step 5: Run format, checksum, formatting, and lint tests**

Run: `cargo test storage::checksum storage::wal::format -- --nocapture`

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/storage/mod.rs src/storage/checksum.rs src/storage/wal
git commit -m "feat(wal): define checksummed WAL formats"
```

---

### Task 4: Add WAL file lifecycle and cross-process database locking

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/storage/file.rs`
- Modify: `src/storage/test_support.rs`
- Modify: `src/storage/wal/mod.rs`

**Interfaces:**
- Produces: `wal_path(&Path) -> PathBuf` without lossy UTF-8 conversion.
- Produces: `Wal::{create, open, path, header, append_record, read_exact_at, sync_all, len, truncate}`.
- Produces: main-file lifetime lock through `fs4::FileExt::try_lock`.

- [ ] **Step 1: Add failing WAL lifecycle and lock tests**

Add these concrete assertions:

```rust
#[test]
fn create_writes_two_synced_wal_header_slots() {
    let temp = TempDatabase::new("wal-create");
    let header = DatabaseHeader::new(4096, [7; 16]).unwrap();
    let wal = Wal::create(temp.path(), &header).unwrap();
    assert_eq!(std::fs::metadata(wal.path()).unwrap().len(), 8192);
    assert_eq!(wal.header().header_sequence, 2);
}

#[test]
fn wal_path_preserves_non_ascii_os_strings() {
    assert_eq!(wal_path(Path::new("数据.db")), PathBuf::from("数据.db-wal"));
}

#[test]
fn second_database_file_lock_returns_busy() {
    let temp = TempDatabase::new("lock");
    let first = DatabaseFile::create(temp.path(), 4096, [1; 16], DEFAULT_MAX_DATABASE_BYTES).unwrap();
    let error = DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Busy);
    drop(first);
}
```

Update `TempDatabase::drop` to remove only its exact database and `-wal` path. Do not use globs.

Also add `missing_wal_rejects_an_existing_database` and `initial_empty_database_can_rebuild_a_missing_wal`. The first creates and checkpoints one committed page, closes the database, removes only the WAL, and requires `Io` on reopen. The second uses checkpoint 0, page count 1, and empty roots; read-write reopen may recreate the WAL, while read-only reopen must return `Io` and leave the filesystem unchanged.

- [ ] **Step 2: Run focused tests and verify failure**

Run: `cargo test storage::wal::tests::create_writes_two_synced_wal_header_slots -- --exact`

Expected: failure because `Wal::create` is absent.

- [ ] **Step 3: Pin and use the locking dependency**

Add exactly:

```toml
[dependencies]
fs4 = { version = "=1.1.0", default-features = false, features = ["sync"] }
```

After opening or creating the main `File`, call:

```rust
use fs4::{FileExt, TryLockError};

match FileExt::try_lock(&file) {
    Ok(()) => {}
    Err(TryLockError::WouldBlock) => return Err(DbError::new(ErrorCode::Busy, "database file is locked by another process")),
    Err(TryLockError::Error(error)) => return Err(io_error("lock database", error)),
}
```

Let dropping the owning file release the lock; do not duplicate and independently lock the same file.

- [ ] **Step 4: Implement bounded WAL file operations**

`Wal::create` writes Slot A sequence 1 and syncs, writes Slot B sequence 2 and syncs. `Wal::open` independently validates both slots and selects the highest sequence matching the requested database UUID, page size, and superblock generation. Keep `end_offset` from file metadata and require `len >= 8192` before record scanning.

Every write, sync, and truncate must pass through named `FaultPoint` hooks rather than calling the OS directly from higher layers.

- [ ] **Step 5: Run lifecycle tests on the host platform**

Run: `cargo test storage::file storage::wal::tests -- --nocapture`

Expected: all pass; lock contention returns code 1400; WAL creation leaves no partial success state.

- [ ] **Step 6: Verify MSRV dependency resolution**

Run: `rustup run 1.85.0 cargo test storage::wal::tests -- --nocapture`

Expected: pass with Rust 1.85. Remove the generated `Cargo.lock` before committing.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/storage/file.rs src/storage/test_support.rs src/storage/wal/mod.rs
git commit -m "feat(storage): add WAL files and process locking"
```

---

### Task 5: Build the committed-version index and strict recovery scanner

**Files:**
- Create: `src/storage/wal/index.rs`
- Create: `src/storage/wal/recovery.rs`
- Modify: `src/storage/wal/mod.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Produces: `WalVersion { lsn, offset, generation }`.
- Produces: `WalIndex::{publish, visible_version, state_at, discard_through}`.
- Produces: `RecoveryResult { latest_lsn, latest_state, index, committed_end, truncate_to }`.
- Produces: `Wal::recover(&DatabaseHeader, OpenMode) -> Result<RecoveryResult>`.

- [ ] **Step 1: Write index visibility tests**

Use one page with versions at LSN 2, 4, and 7 and assert snapshots 1, 2, 6, and 7 resolve to none, 2, 4, and 7. Add a second test proving `state_at` returns the exact commit roots/page count for the requested LSN and `discard_through(4)` retains only versions needed after checkpoint 4.

- [ ] **Step 2: Write table-driven recovery tests**

Create helper `encoded_transaction(lsn, previous_lsn, pages, state) -> Vec<u8>`. Cover:

- two valid consecutive commits;
- EOF in record header, payload, and padding;
- complete frames without Commit;
- bad header CRC, payload CRC, transaction CRC, ordinal, page order, duplicate page, previous LSN, and generation;
- old-generation record area after a reset header;
- current-generation record after an old-generation suffix.

Read-write recovery must truncate only permitted final incomplete transactions and sync the truncation. Read-only recovery must leave file length unchanged.

- [ ] **Step 3: Run index and recovery tests and verify failure**

Run: `cargo test storage::wal::index storage::wal::recovery -- --nocapture`

Expected: compilation fails because the modules do not exist.

- [ ] **Step 4: Implement `WalIndex` with checked publication**

Use:

```rust
pub(super) struct WalIndex {
    pages: BTreeMap<PageId, Vec<WalVersion>>,
    states: BTreeMap<Lsn, CommitState>,
}

pub(super) fn visible_version(&self, page_id: PageId, snapshot: Lsn) -> Option<WalVersion>;
pub(super) fn state_at(&self, checkpoint: &DatabaseHeader, snapshot: Lsn) -> Result<CommitState>;
```

Reject non-increasing per-page LSN, duplicate commit LSN, missing state, and offset overflow before mutating either map. Publish one transaction atomically by validating into temporary vectors first.

- [ ] **Step 5: Implement the strict transaction scanner**

Scan from offset 8192 with an explicit `PendingTransaction`. Validate every record before allocating its payload, cap payload by page size/64-byte commit size, and compute transaction CRC incrementally. Do not publish PageFrames until the matching Commit validates.

Classify only unexpected EOF in the final pending transaction as truncatable. A complete record with invalid fields or CRC is `Corruption`. If the first record belongs to the previous WAL generation after the superblock has published the new generation, require the entire record area to be old generation before returning `truncate_to = Some(8192)`.

- [ ] **Step 6: Run recovery tests and the existing corruption suite**

Run: `cargo test storage::wal storage::test_support::truncation_and_restamped_field_corruption_return_controlled_errors -- --nocapture`

Expected: all pass without panic and with deterministic error codes.

- [ ] **Step 7: Commit**

```bash
git add src/storage/wal src/storage/test_support.rs
git commit -m "feat(wal): recover committed page versions"
```

---

### Task 6: Add snapshot registration and version-aware caching

**Files:**
- Create: `src/storage/snapshot.rs`
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/cache.rs`

**Interfaces:**
- Produces: `SnapshotRegistry::{register, oldest, is_empty, lock_empty}` plus RAII `SnapshotLease` and `EmptySnapshotGuard`.
- Produces: `CacheKey { page_id, source_lsn }`.
- Changes: `PageCache::{get, remove}` accept `CacheKey`; capacity remains a total-page-version limit.

- [ ] **Step 1: Write snapshot lifetime tests**

Add tests that register two leases at LSN 3 and one at LSN 8, verify `oldest() == Some(Lsn(3))`, drop them one by one, and verify reference counts and final emptiness. Add `fn assert_send_sync<T: Send + Sync>() {}` and invoke it for `SnapshotLease`, `PageHandle`, and `PageCache`.

- [ ] **Step 2: Write versioned-cache tests**

Insert `(PageId(5), Lsn(2))` and `(PageId(5), Lsn(7))` with different bytes. Assert both can coexist, eviction uses the combined capacity, and a pinned old version does not block eviction of an unpinned new version.

- [ ] **Step 3: Run focused tests and verify failure**

Run: `cargo test storage::snapshot storage::cache -- --nocapture`

Expected: compilation fails because snapshot and cache-key types are absent.

- [ ] **Step 4: Implement registry and RAII release**

Use an `Arc<Mutex<SnapshotCounts>>` shared by `SnapshotRegistry` and `SnapshotLease`; `Drop` decrements exactly once. `lock_empty(&self) -> Result<EmptySnapshotGuard<'_>>` acquires the same mutex, returns `Busy` if counts are nonempty, and otherwise retains the guard so new registrations cannot enter during WAL reset. Map poisoned mutexes to `Internal`. Use `checked_add` for counts and treat an impossible missing count during Drop as an internal invariant recorded by an invalid registry state rather than panicking.

- [ ] **Step 5: Implement cache keys and update callers with checkpoint LSN 0**

Define:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct CacheKey {
    pub(super) page_id: PageId,
    pub(super) source_lsn: Lsn,
}
```

Until Pager WAL integration in Task 7, existing main-file reads use `source_lsn: Lsn(0)`. Remove by key, not by PageId, and add `remove_page_versions(PageId)` for commit/checkpoint transitions.

- [ ] **Step 6: Run all fast tests and commit**

Run: `cargo test --all-targets --all-features`

Expected: 187 passed, 0 failed, 2 ignored plus the new tests.

```bash
git add src/storage/mod.rs src/storage/snapshot.rs src/storage/cache.rs src/storage/pager.rs
git commit -m "feat(storage): track snapshots and page versions"
```

---

### Task 7: Route Pager reads and commits through the recovered WAL

**Files:**
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/btree/mod.rs`
- Modify: `src/storage/btree/search.rs`
- Modify: `src/storage/btree/mutate.rs`
- Modify: `src/storage/overflow.rs`
- Modify: `src/storage/validate.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Produces: cloneable `Pager` backed by `Arc<PagerInner>`.
- Produces: `Pager::begin_read(&self) -> Result<ReadSnapshot>`.
- Changes: `Pager::begin_write(&self) -> Result<WriteBatch<'_>>` holds the unique writer guard.
- Produces: `Pager::try_begin_write(&self) -> Result<WriteBatch<'_>>`, mapping writer contention to `Busy`.
- Produces: `Pager::latest_lsn(&self) -> Result<Lsn>` for internal verification and later transaction consumers.
- Produces: `PageSource for ReadSnapshot` and `PageSource for WriteBatch`.
- Changes: `WriteBatch::commit` appends and syncs WAL, then publishes the index/state; it does not write the main database.

- [ ] **Step 1: Add commit durability and snapshot-isolation tests**

Add four tests: `commit_changes_only_the_wal_until_checkpoint`, `synced_commit_reopens_from_wal`, `old_snapshot_keeps_old_page_after_new_commit`, and `wal_sync_failure_is_not_published_and_blocks_later_writes`. In the first, capture `std::fs::read(temp.path())` before commit, commit one page, assert the main bytes are identical, and assert `metadata(wal_path(temp.path())).len() > 8192`. In the reopen test, drop Pager, reopen, require `latest_lsn() == Lsn(1)`, and read the same catalog root/value. In the snapshot test, hold the LSN-1 snapshot across an LSN-2 replacement and assert old/new bytes differ exactly as committed. In the fault test, inject `BeforeWalSync`, assert `latest_lsn()` is unchanged, the old snapshot value remains readable, and assert a later `try_begin_write()` returns `ErrorCode::Transaction` with recovery-required context.

Add `lsn_overflow_rejects_before_wal_io`: seed recovered state at `Lsn(u64::MAX)`, capture WAL bytes, attempt a nonempty commit, require `ErrorCode::Transaction`, and assert the WAL bytes are unchanged.

- [ ] **Step 2: Run the focused tests and verify they fail**

Run: `cargo test storage::pager::tests::commit_changes_only_the_wal_until_checkpoint -- --exact`

Expected: failure because the current commit writes the main file directly.

- [ ] **Step 3: Introduce shared Pager state with explicit locks**

Use this responsibility split:

```rust
struct PagerInner {
    main: Mutex<DatabaseFile>,
    wal: Mutex<Wal>,
    published: RwLock<PublishedState>,
    snapshots: SnapshotRegistry,
    cache: Mutex<PageCache>,
    writer: Mutex<()>,
    mode: OpenMode,
    max_database_bytes: u64,
    max_wal_bytes: u64,
}

struct PublishedState {
    checkpoint: DatabaseHeader,
    latest_lsn: Lsn,
    latest: CommitState,
    wal_index: WalIndex,
    write_state: WriteState,
}

enum WriteState { Ready, NeedsRecovery }
```

Opening creates/opens the WAL, runs recovery, validates the recovered catalog/freelist through a snapshot, and publishes the recovered state only after all checks pass.

- [ ] **Step 4: Implement `ReadSnapshot` and WAL-first page resolution**

`begin_read` acquires the published-state read lock, captures `latest_lsn`, and keeps that read lock until the same LSN has been registered in `SnapshotRegistry`; this closes the race with checkpoint publication/reset. It then releases both locks before returning. `ReadSnapshot::read_page` resolves a `WalVersion`, forms `CacheKey { page_id, source_lsn }`, reads and revalidates the WAL record on cache miss, or reads the main file using the checkpoint LSN key.

Do not hold the published-state lock while taking WAL/main/cache locks. Copy only the resolved offset, generation, state, and checkpoint LSN out of the lock.

- [ ] **Step 5: Change `WriteBatch` to use a base LSN and writer guard**

`WriteBatch<'a>` holds `&'a Pager`, `MutexGuard<'a, ()>`, `base_lsn`, committed `CommitState`, and existing private mutation maps. Reads first use dirty pages, then call the same version resolver at `base_lsn`. Preserve all pre-I/O structural validation from Phase 2.

- [ ] **Step 6: Implement the exact WAL commit boundary**

After validation, collect prepared frames sorted by PageId, calculate the candidate `CommitState`, preflight the 1 GiB transaction/WAL limit, and call a single `Wal::append_transaction`. After `sync_all` succeeds, acquire the published-state write lock and atomically publish all returned `WalVersion` entries, CommitState, and LSN. Then update cache entries and return success.

On any WAL append/sync error, publish only `WriteState::NeedsRecovery`; never publish frames or LSN. Empty transactions retain the current LSN and do no I/O.

- [ ] **Step 7: Adapt B+Tree callers to explicit snapshots**

Tests and internal read flows must use:

```rust
let mut read = pager.begin_read()?;
let value = tree.get(&mut read, key)?;
```

Mutation flows continue to pass `&mut WriteBatch`. Remove `PageSource for Pager` so a multi-call operation cannot accidentally observe different latest LSN values.

- [ ] **Step 8: Run the complete fast suite**

Run: `cargo test --all-targets --all-features`

Expected: all non-ignored tests pass; all former normal-close persistence tests now prove WAL reopen persistence.

- [ ] **Step 9: Commit**

```bash
git add src/storage
git commit -m "feat(storage): commit and read through WAL snapshots"
```

---

### Task 8: Implement safe checkpoint publication and WAL generation reset

**Files:**
- Create: `src/storage/checkpoint.rs`
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/wal/mod.rs`
- Modify: `src/storage/wal/index.rs`
- Modify: `src/storage/file.rs`
- Modify: `src/fault.rs`

**Interfaces:**
- Produces: `Pager::checkpoint(&self) -> Result<CheckpointResult>`.
- Produces: `CheckpointResult { from_lsn, to_lsn, pages_written, wal_reset }`.
- Produces: `Wal::begin_generation_reset`, `Wal::finish_generation_reset`.
- Produces: automatic checkpoint attempt before a new write once WAL reaches 64 MiB.
- Changes: `PagerConfig` gains `checkpoint_wal_bytes` and `max_wal_bytes`, defaulting to 64 MiB and 1 GiB; tests use small positive limits instead of allocating GiB files.

- [ ] **Step 1: Write checkpoint state tests**

Cover:

- no readers: checkpoint to latest, main file contains values, WAL becomes exactly 8192 bytes;
- reader at LSN 2 with latest LSN 4: checkpoint to 2, WAL is not truncated, both snapshots remain correct;
- checkpoint called again without changes: zero pages written and deterministic result;
- all readers released: second checkpoint resets WAL and increments generation;
- each data-write, main-sync, superblock-write, WAL-header-write, WAL-sync, and truncate fault leaves a reopenable old or new complete state.

- [ ] **Step 2: Run the first checkpoint test and verify failure**

Run: `cargo test storage::checkpoint::tests::checkpoint_without_readers_publishes_latest_and_resets_wal -- --exact`

Expected: compilation fails because `storage::checkpoint` is absent.

- [ ] **Step 3: Implement target selection and page materialization**

Under the writer lock, copy `checkpoint_lsn`, `latest_lsn`, oldest snapshot, target CommitState, and all latest page versions in `(checkpoint, target]`. Release state locks, read each selected WAL frame, extend the main file to target page count, write each page in PageId order, and sync the main file.

- [ ] **Step 4: Publish the alternate superblock**

Build the target header with incremented checked `publish_sequence`, target roots/page count, and unchanged WAL generation. Call `DatabaseFile::publish_superblock`, which writes and syncs only the inactive slot. Then update the published checkpoint and prune only in-memory versions at or before target that no active snapshot requires.

- [ ] **Step 5: Implement the two-header WAL reset protocol**

If target equals latest, call `lock_empty`. When it returns `Busy`, skip physical reset and return `wal_reset = false`; this is not a checkpoint failure. When it returns `EmptySnapshotGuard`, retain that guard to block new registrations, write/sync an inactive WAL Header Slot with generation + 1 and base checkpoint latest, publish/sync the matching main Superblock, truncate WAL to 8192 and sync, then clear obsolete index/state/cache entries. Release the snapshot guard last.

If the process stops after the new WAL header but before the main superblock, opening selects the old generation. If it stops after the main superblock but before truncate, recovery recognizes the old-generation record suffix and truncates it.

- [ ] **Step 6: Add thresholds and hard-limit behavior**

Before granting a writer, if WAL length is at least 64 MiB, attempt checkpoint. If the projected transaction crosses 1 GiB and an old snapshot prevents reset, return `Busy` with the oldest LSN. If the transaction alone exceeds 1 GiB, return `Transaction` before WAL I/O.

- [ ] **Step 7: Run checkpoint, reopen, and model tests**

Run: `cargo test storage::checkpoint storage::pager storage::test_support::reference_model_commits_once_per_persistence_window -- --nocapture`

Expected: all pass; every injected checkpoint error reopens to an intact committed state.

- [ ] **Step 8: Commit**

```bash
git add src/fault.rs src/storage
git commit -m "feat(storage): checkpoint WAL safely"
```

---

### Task 9: Prove multi-reader/single-writer behavior and process locking

**Files:**
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/snapshot.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Verifies: `Pager`, `ReadSnapshot`, and completed immutable row/page handles are usable across threads as designed.
- Verifies: writer lock order is writer → state → snapshot → cache, with no reverse acquisition.
- Verifies: lock contention from another process maps to `Busy`.

- [ ] **Step 1: Add a deterministic threaded reference-model test**

Use `Arc<Pager>`, `Barrier`, and scoped threads. Keep one reader at LSN 1 while a writer commits LSN 2 through 20 and another thread repeatedly opens new snapshots. Assert the old reader always sees LSN-1 values, new readers see monotonically nondecreasing committed states, and the final snapshot exactly matches a `BTreeMap` reference model.

- [ ] **Step 2: Add writer-contention and checkpoint-progress tests**

One thread must hold an unfinished `WriteBatch`; a second `try_begin_write` must return `Busy` without blocking. Readers must still complete. A checkpoint attempt must serialize behind the writer and complete after the first batch rolls back.

- [ ] **Step 3: Add a real child-process lock test**

Add an ignored helper test `database_lock_child` that opens the path from `ORA_SQLLITE_LOCK_CHILD_PATH` and exits with code 0 only when it receives `Busy`. The parent invokes the current test executable with:

```rust
Command::new(std::env::current_exe()?)
    .args(["--exact", "storage::test_support::database_lock_child", "--ignored"])
    .env("ORA_SQLLITE_LOCK_CHILD_PATH", temp.path())
    .status()?;
```

Run it once while the parent Pager is open and once after drop; the first child must observe `Busy`, the second must open successfully under a separate helper mode.

- [ ] **Step 4: Run concurrency tests repeatedly**

Run: `cargo test storage::test_support::multi_reader_single_writer_matches_reference_model -- --exact --nocapture`

Run the same command 10 times in the shell test loop appropriate to the host.

Expected: no deadlock, timeout, LSN regression, or result mismatch.

- [ ] **Step 5: Run Windows-relevant file cleanup tests**

Run: `cargo test temporary_database database_lock -- --nocapture`

Expected: all file handles release; exact database and WAL paths can be removed after Pager drop.

- [ ] **Step 6: Commit**

```bash
git add src/storage/pager.rs src/storage/snapshot.rs src/storage/test_support.rs
git commit -m "test(storage): prove multi-reader single-writer semantics"
```

---

### Task 10: Add subprocess crash recovery and corruption matrices

**Files:**
- Modify: `src/fault.rs`
- Modify: `src/storage/test_support.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: deterministic named fault boundaries for WAL append/sync, main-page checkpoint, superblock publication, WAL-header publication, and truncation.
- Produces: ignored `phase_3_crash_recovery_matrix` test for dedicated Linux CI.

- [ ] **Step 1: Replace generic file fault points with operation-specific points**

Define stable variants including:

```rust
BeforeWalRecordWrite,
AfterWalRecordWrite,
BeforeWalSync,
AfterWalSync,
BeforeMainPageWrite,
AfterMainPageWrite,
BeforeMainSync,
AfterMainSync,
BeforeSuperblockWrite,
AfterSuperblockWrite,
BeforeWalHeaderWrite,
AfterWalHeaderWrite,
BeforeWalTruncate,
AfterWalTruncate,
```

Keep fault injection deterministic by point plus occurrence number. Update existing tests to assert the exact event order.

- [ ] **Step 2: Build the child crash helper**

Add ignored test `phase_3_crash_child`. It reads a scenario file path, fault point, and occurrence from environment variables, opens the fixture, installs a fault injector that calls `std::process::abort()` at the chosen boundary, and performs exactly one commit or checkpoint. The parent must never reuse the child process's open handles.

- [ ] **Step 3: Build the parent crash matrix**

For every observed occurrence in a successful commit and checkpoint trace:

1. Copy a fresh valid fixture to an exact temporary database and WAL pair.
2. Run the crash child at that occurrence.
3. Reopen in the parent.
4. Assert the logical state equals either the complete pre-operation model or the complete post-operation model.
5. For a child abort at `AfterWalSync` or any later commit boundary, require the post-operation model; before WAL sync, accept only the complete pre-operation or complete post-operation model.
6. Run a second reopen and checkpoint and assert byte/logical idempotence.

Add direct corruption cases for full-sized bad CRC, bad LSN, bad generation, valid old-generation suffix followed by current-generation bytes, and both main Superblocks invalid. All must return `Corruption`, never truncate silently.

- [ ] **Step 4: Run the crash matrix locally once**

Run: `cargo test phase_3_crash_recovery_matrix -- --ignored --nocapture`

Expected: all fault positions pass; child abort exit codes are expected and inspected by the parent.

- [ ] **Step 5: Add a dedicated CI job**

Add `phase3-crash-recovery` on `ubuntu-latest`, timeout 240 minutes, installing stable Rust and running exactly the ignored matrix. Keep fast Linux, Rust 1.85, Windows, and Phase 2 exhaustive jobs unchanged.

- [ ] **Step 6: Run workflow syntax and fast tests**

Run: `cargo test --all-targets --all-features`

Run: `git diff --check`

Expected: all fast tests pass and the workflow has no whitespace errors.

- [ ] **Step 7: Commit**

```bash
git add src/fault.rs src/storage/test_support.rs .github/workflows/ci.yml
git commit -m "test(storage): add WAL crash recovery matrix"
```

---

### Task 11: Update Phase 3 status and run release-grade verification

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-09-05-phase-3-wal-transactions-design.md` only if implementation discovered an approved clarification

**Interfaces:**
- Produces: accurate user-facing status and explicit remaining Phase 4 boundary.
- Verifies: every Phase 3 acceptance criterion in the spec.

- [ ] **Step 1: Update README status and limitations**

Replace the Phase 2 no-WAL warning with text stating that Phase 3 implements full-durability WAL commits, deterministic recovery, transaction-level read snapshots, one writer, checkpointing, and single-process file ownership. Keep this explicit limitation:

```text
SQL, the system catalog schema, and the public database CRUD API remain unimplemented.
Phase 3 supports multiple readers and one writer only within one process; a second
process opening the same database receives Busy.
```

State that format-1 Phase 2 development databases must be recreated.

- [ ] **Step 2: Run formatting and lint gates**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: both pass with no warnings.

- [ ] **Step 3: Run stable and MSRV fast suites**

Run: `cargo test --all-targets --all-features`

Run: `rustup run 1.85.0 cargo test --all-targets --all-features`

Expected: both pass; only the documented exhaustive, scale, and crash-matrix tests are ignored in the fast suite.

- [ ] **Step 4: Run exhaustive storage and crash suites**

Run: `cargo test fixed_seed_reference_model_covers_every_page_size_and_tree_kind -- --ignored --nocapture`

Run: `cargo test phase_3_crash_recovery_matrix -- --ignored --nocapture`

Expected: all six storage configurations and every crash boundary pass.

- [ ] **Step 5: Verify repository invariants**

Run: `rg -n "unsafe" src`

Expected: only `#![forbid(unsafe_code)]`; no unsafe block.

Run: `rg -n -i "SQL parser|public Database|pub struct Database|savepoint|mmap|async" src`

Expected: no Phase 4 implementation or forbidden mechanism.

Run: `git status --short --branch`

Expected: only README/spec changes intended for the final commit; no `Cargo.lock`, database, WAL, or temporary files.

- [ ] **Step 6: Commit documentation**

```bash
git add README.md docs/superpowers/specs/2026-09-05-phase-3-wal-transactions-design.md
git commit -m "docs: record phase 3 WAL guarantees"
```

- [ ] **Step 7: Perform final diff review**

Run: `git diff --check origin/main...HEAD`

Run: `git log --oneline origin/main..HEAD`

Confirm the diff contains only Phase 3 WAL, transactions, tests, CI, and documentation; no public CRUD or SQL surface is present.
