# ORA-SQLLite Phase 2 Storage Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a private, persistent single-file Pager and byte-key B+Tree that supports complete mutation, bounded caching, overflow values, page reuse, normal-close reopen, and controlled corruption failures.

**Architecture:** `DatabaseFile` owns exact synchronous file I/O; `Pager` validates pages and owns the bounded clean-page cache; an exclusive `WriteBatch` owns copy-on-write dirty images, allocation, release, and header publication. Slotted pages, overflow chains, and the SQLite-style freelist are disk-format components beneath a unique byte-key B+Tree; a separate validator checks the tree, leaf chain, overflow pages, and freelist before commit and in tests.

**Tech Stack:** Rust 1.85+, edition 2024, Rust standard library only, Cargo test/clippy/rustfmt, Windows and Linux filesystem APIs through `std::fs::File`.

**Spec:** `docs/superpowers/specs/2026-09-02-phase-2-storage-engine-design.md`

## Global Constraints

- Keep `#![forbid(unsafe_code)]`, Rust edition 2024, and `rust-version = "1.85"`.
- Add no runtime or development dependencies; use only the Rust standard library.
- Support page sizes 4096, 8192, and 16384 bytes, with PageId 0 reserved for the database header.
- Default maximum database size is 10 GiB and default cache capacity is 1024 pages.
- Persist every integer in fixed little-endian form and protect every ordinary page with CRC32C over the full page while bytes 36..40 are zero.
- Keep every Phase 2 storage interface crate-private; do not re-export Pager or BTree from `src/lib.rs`.
- Support byte-ordered unique keys of at most 1024 bytes.
- Guarantee normal commit/close/reopen persistence only; do not claim crash atomicity until Phase 3 WAL.
- Allow only one write batch and no threads, file locks, mmap, async I/O, SQL, catalog schema, public CRUD API, or concurrency behavior.
- Once a write or sync operation has started and returns an error, poison that Pager instance.
- Use test-first red-green-refactor cycles and end each task with a focused commit.
- Keep the CI foundation change in PR #2 separate; rebase Phase 2 on the merged CI commit before the final Phase 2 gate.

## File Map

- `src/storage/mod.rs`: private module routing plus `PageId`, `PageRef`, `Lsn`, constants, and shared checked conversions.
- `src/storage/checksum.rs`: dependency-free CRC32C plus whole-page checksum verification and stamping.
- `src/storage/header.rs`: database-header decoding, open-version policy, read-only accessors, and restricted root updates.
- `src/storage/page.rs`: page-header accessors/mutators and `PageFrame`, the validated full-page in-memory representation.
- `src/storage/file.rs`: create-new/open, page-count and checked-offset calculation, exact positioned page I/O, extension, and sync.
- `src/storage/cache.rs`: deterministic capacity-bounded LRU of clean `PageHandle` values with pin protection.
- `src/storage/pager.rs`: validated reads, Pager state, private dirty copies, commit ordering, poisoning, and header publication.
- `src/storage/slotted.rs`: checked slot-directory parsing, insertion, deletion, compaction, and free-space calculation.
- `src/storage/freelist.rs`: trunk/leaf encoding and batch-local page release/reuse rules.
- `src/storage/overflow.rs`: multi-page value write/read/free and chain validation.
- `src/storage/btree/mod.rs`: `BTree`, `TreeKind`, node/cell codecs, root ownership, and module routing.
- `src/storage/btree/search.rs`: point lookup, child routing, lower-bound positioning, and streaming range cursor.
- `src/storage/btree/mutate.rs`: insert/delete, split propagation, borrowing, merging, and root growth/shrink.
- `src/storage/validate.rs`: structural traversal and cross-set validation for tree, leaf chain, overflow, and freelist pages.
- `src/storage/test_support.rs`: test-only temporary paths and Pager fixtures, later extended with deterministic PRNG, fault injector, corruption helpers, and model runner.
- `README.md`: current Phase 2 capability and the explicit no-WAL crash-safety limitation.

---

### Task 1: Harden disk-format primitives for Pager use

**Files:**
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/checksum.rs`
- Modify: `src/storage/header.rs`
- Modify: `src/storage/page.rs`
- Test: unit tests in the four files

**Interfaces:**
- Produces: `PageRef::new(page_id: PageId, generation: u64) -> Result<PageRef>` and read-only accessors.
- Produces: `DatabaseHeader::{page_size, uuid, catalog_root, freelist_root, minor, features}` plus crate-restricted `set_catalog_root` and `set_freelist_root`.
- Produces: `DatabaseHeader::validate_open(mode: OpenMode) -> Result<()>`.
- Produces: `PageHeader` getters and invariant-preserving setters used by Pager and slotted pages.
- Produces: `verify_page_checksum(bytes: &[u8]) -> Result<()>` and `stamp_page_checksum(bytes: &mut [u8]) -> Result<u32>`.

- [ ] **Step 1: Write failing PageRef and checksum tests**

Add tests that pin down generation and checksum behavior:

```rust
#[test]
fn page_ref_rejects_header_page_and_zero_generation() {
    assert_eq!(PageRef::new(PageId(0), 1).unwrap_err().code(), ErrorCode::Corruption);
    assert_eq!(PageRef::new(PageId(1), 0).unwrap_err().code(), ErrorCode::Corruption);
    assert_eq!(PageRef::new(PageId(7), 3).unwrap().page_id(), PageId(7));
}

#[test]
fn whole_page_checksum_ignores_only_its_own_field() {
    let mut page = vec![0x5a; 4096];
    page[36..40].fill(0);
    let stored = stamp_page_checksum(&mut page).expect("stamp");
    assert_eq!(stored, u32::from_le_bytes(page[36..40].try_into().unwrap()));
    verify_page_checksum(&page).expect("valid page");
    page[100] ^= 1;
    assert_eq!(verify_page_checksum(&page).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Run the focused tests and confirm failure**

Run: `cargo test --lib storage::checksum storage::page`

Expected: compilation fails because `PageRef`, `stamp_page_checksum`, and `verify_page_checksum` do not exist.

- [ ] **Step 3: Add the shared reference types and whole-page CRC helpers**

Move the CRC32C implementation from `storage/mod.rs` into `checksum.rs`, re-export it only to sibling storage modules, and add these exact type shapes:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct PageId(pub(super) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct PageRef {
    page_id: PageId,
    generation: u64,
}

impl PageRef {
    pub(super) fn new(page_id: PageId, generation: u64) -> Result<Self>;
    pub(super) const fn page_id(self) -> PageId;
    pub(super) const fn generation(self) -> u64;
}

pub(super) fn stamp_page_checksum(bytes: &mut [u8]) -> Result<u32>;
pub(super) fn verify_page_checksum(bytes: &[u8]) -> Result<()>;
```

Both checksum functions require `bytes.len()` to be one of the three supported page sizes. Copy bytes 36..40 aside, zero them while computing, and restore or replace them without changing any other byte.

- [ ] **Step 4: Write failing header policy and accessor tests**

```rust
#[test]
fn future_minor_is_read_only_when_features_are_known() {
    let mut bytes = DatabaseHeader::new(4096, [9; 16]).unwrap().encode();
    bytes[18..20].copy_from_slice(&1u16.to_le_bytes());
    refresh_database_crc(&mut bytes);
    let header = DatabaseHeader::decode(&bytes).expect("structurally valid");
    header.validate_open(OpenMode::ReadOnly).expect("safe read-only open");
    assert_eq!(header.validate_open(OpenMode::ReadWrite).unwrap_err().code(), ErrorCode::Unsupported);
}

#[test]
fn page_header_mutators_preserve_bounds() {
    let mut header = PageHeader::new(PageType::TableLeaf, PageId(1), 1, 4096).unwrap();
    header.set_layout(2, 56, 4000).expect("valid layout");
    assert_eq!(header.cell_count(), 2);
    assert_eq!(header.set_layout(2, 4001, 4000).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 5: Implement restricted header APIs and version policy**

Define `OpenMode` in `file.rs` when Task 2 begins; until then define it in `storage/mod.rs` and move it without changing the enum. Required API:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenMode { ReadOnly, ReadWrite }

impl DatabaseHeader {
    pub(super) const fn page_size(&self) -> u32;
    pub(super) const fn uuid(&self) -> [u8; 16];
    pub(super) const fn minor(&self) -> u16;
    pub(super) const fn features(&self) -> u64;
    pub(super) const fn catalog_root(&self) -> PageId;
    pub(super) const fn freelist_root(&self) -> PageId;
    pub(super) fn set_catalog_root(&mut self, root: PageId);
    pub(super) fn set_freelist_root(&mut self, root: PageId);
    pub(super) fn validate_open(&self, mode: OpenMode) -> Result<()>;
}
```

`decode` continues to reject unknown major versions, invalid page sizes, bad CRC, and reserved bytes. It accepts a future minor only when feature bits are all known; with the Phase 2 known mask equal to zero, any nonzero feature bit is corruption. `validate_open` returns `Unsupported` only for a future minor opened read-write.

Add `PageHeader` getters for every persisted field and setters for checksum, generation, right sibling, and `(cell_count, slot_end, payload_start)`. Every setter checks bounds before mutation.

- [ ] **Step 6: Verify and commit**

Run:

```powershell
cargo fmt --check
cargo test --lib storage::checksum storage::header storage::page
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all commands exit 0.

Commit:

```powershell
git add src/storage/mod.rs src/storage/checksum.rs src/storage/header.rs src/storage/page.rs
git commit -m "feat: harden storage page primitives"
```

### Task 2: Implement exact single-file database I/O

**Files:**
- Create: `src/storage/file.rs`
- Create: `src/storage/test_support.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in `src/storage/file.rs`

**Interfaces:**
- Consumes: `DatabaseHeader`, `OpenMode`, and supported page-size checks from Task 1.
- Produces: `DatabaseFile::{create, open, page_size, page_count, header, read_page, write_page, extend_to_pages, write_header, sync_all}`.
- Produces: contextual `Io` and `Corruption` errors with operation and PageId in messages.
- Produces for tests: `TempDatabase::new(label)`, `TempDatabase::path()`, and exact-file cleanup in `Drop`.

- [ ] **Step 1: Write failing create/open and offset tests**

```rust
#[test]
fn create_is_exclusive_and_writes_one_synced_header_page() {
    let temp = TempDatabase::new("create");
    let db = DatabaseFile::create(temp.path(), 4096, [4; 16], 10 * 1024 * 1024).unwrap();
    assert_eq!(db.page_count(), 1);
    assert_eq!(std::fs::metadata(temp.path()).unwrap().len(), 4096);
    assert_eq!(DatabaseFile::create(temp.path(), 4096, [4; 16], 10 * 1024 * 1024).unwrap_err().code(), ErrorCode::Io);
    drop(db);
}

#[test]
fn checked_offset_rejects_overflow_and_limit() {
    assert_eq!(checked_page_offset(PageId(u64::MAX), 16384, u64::MAX).unwrap_err().code(), ErrorCode::Corruption);
    assert_eq!(checked_page_offset(PageId(3), 4096, 8192).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run: `cargo test --lib storage::file`

Expected: compilation fails because module `file` and `DatabaseFile` are missing.

- [ ] **Step 3: Implement create and open**

Use `OpenOptions::create_new(true).read(true).write(true)` for create. Encode the 128-byte header into a zero-filled full header page, write it with `write_all`, then `sync_all`. Open read-only with `.read(true)` and read-write with `.read(true).write(true)`.

Required constructor surface:

```rust
pub(super) const DEFAULT_MAX_DATABASE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

impl DatabaseFile {
    pub(super) fn create(path: &Path, page_size: u32, uuid: [u8; 16], max_bytes: u64) -> Result<Self>;
    pub(super) fn open(path: &Path, mode: OpenMode, max_bytes: u64) -> Result<Self>;
}
```

Open first reads exactly 128 bytes, decodes the header and page size, validates open mode, then checks `len >= page_size`, `len % page_size == 0`, `len <= max_bytes`, and both root PageIds are zero or less than page count.

- [ ] **Step 4: Write failing exact page-I/O tests**

```rust
#[test]
fn page_io_requires_full_pages_and_valid_bounds() {
    let temp = TempDatabase::new("page-io");
    let mut db = DatabaseFile::create(temp.path(), 4096, [1; 16], 8192).unwrap();
    db.extend_to_pages(2).unwrap();
    let page = vec![0xa5; 4096];
    db.write_page(PageId(1), &page).unwrap();
    assert_eq!(db.read_page(PageId(1)).unwrap(), page);
    assert_eq!(db.read_page(PageId(2)).unwrap_err().code(), ErrorCode::Corruption);
    assert_eq!(db.extend_to_pages(3).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 5: Implement exact positioned I/O and sync boundaries**

Implement `read_page` and `write_page` using `seek(SeekFrom::Start(offset))` followed by `read_exact` or `write_all`. Reject PageId 0 in ordinary-page methods, wrong buffer lengths, arithmetic overflow, reads beyond `page_count`, and extension beyond `max_bytes`. Wrap every OS error with operation text such as `"read page 7"`; never collapse short reads into an empty page.

Add fault checks immediately before and after each write and sync:

```rust
self.faults.check(FaultPoint::BeforeFileWrite)?;
self.file.write_all(bytes).map_err(|e| io_error("write page", e))?;
self.faults.check(FaultPoint::AfterFileWrite)?;
```

Constructors default to `Arc::new(NoFaults)` and expose a `#[cfg(test)] with_faults` constructor.

In `test_support.rs`, build filenames from `std::env::temp_dir()`, process ID, a sanitized test label, and a global `AtomicU64`. `TempDatabase::new` removes no pre-existing path; it loops to a fresh candidate. Its `Drop` removes only that exact file and ignores NotFound. Route the module with `#[cfg(test)] mod test_support;`.

- [ ] **Step 6: Verify and commit**

Run `cargo fmt --check`, `cargo test --lib storage::file`, and `cargo clippy --all-targets --all-features -- -D warnings`.

Expected: create, open-policy, truncation, offset, limit, read-only-write rejection, and exact I/O tests pass on Windows.

Commit: `git add src/storage/mod.rs src/storage/file.rs src/storage/test_support.rs && git commit -m "feat: add exact database file io"`

### Task 3: Add validated page frames and bounded LRU caching

**Files:**
- Modify: `src/storage/page.rs`
- Create: `src/storage/cache.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in `src/storage/page.rs` and `src/storage/cache.rs`

**Interfaces:**
- Produces: `PageFrame::{new_zeroed, decode, bytes, bytes_mut, header}`.
- Produces: `type PageHandle = Rc<RefCell<PageFrame>>`.
- Produces: `PageCache::{new, get, insert_clean, remove, len}` with deterministic LRU behavior.

- [ ] **Step 1: Write failing frame validation tests**

```rust
#[test]
fn frame_decode_checks_crc_id_type_and_generation() {
    let expected = PageRef::new(PageId(5), 2).unwrap();
    let mut frame = PageFrame::new_zeroed(PageType::TableLeaf, expected, 4096).unwrap();
    frame.prepare_for_write().unwrap();
    PageFrame::decode(frame.bytes().to_vec(), PageId(5), Some(PageType::TableLeaf), Some(2)).unwrap();
    assert_eq!(PageFrame::decode(frame.bytes().to_vec(), PageId(6), None, None).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Implement PageFrame**

Store exactly `Vec<u8>` and a decoded `PageHeader`. `decode` first checks supported full length and whole-page checksum, then decodes the header and verifies requested PageId, optional PageType, and optional generation in that order. `prepare_for_write` encodes the current header with checksum zero into bytes 0..48, validates the whole page layout, stamps the CRC, and refreshes the in-memory header checksum.

- [ ] **Step 3: Write failing LRU and pin tests**

```rust
#[test]
fn lru_skips_pinned_pages_and_reports_busy_when_all_are_pinned() {
    let mut cache = PageCache::new(2).unwrap();
    let one = handle(1);
    let two = handle(2);
    cache.insert_clean(one.clone()).unwrap();
    cache.insert_clean(two.clone()).unwrap();
    drop(two);
    cache.get(PageId(1)).unwrap();
    cache.insert_clean(handle(3)).unwrap();
    assert!(cache.get(PageId(1)).is_some());
    let pin_three = cache.get(PageId(3)).unwrap();
    assert_eq!(cache.insert_clean(handle(4)).unwrap_err().code(), ErrorCode::Busy);
    drop(pin_three);
}
```

Define the local `handle(id)` fixture by creating a zeroed 4096-byte `TableLeaf` `PageFrame` with generation 1 and wrapping it in `Rc<RefCell<_>>`; it performs no file I/O.

- [ ] **Step 4: Implement deterministic clean-page LRU**

Use `BTreeMap<PageId, CacheEntry>` and a monotonically increasing `u64` access clock. Each entry stores `PageHandle` and `last_used`. On lookup, increment the clock and update the entry. On insertion at capacity, choose the unpinned entry with smallest `(last_used, page_id)`; `Rc::strong_count(handle) > 1` means pinned. Capacity zero is `Type`; clock overflow renormalizes entries in current LRU order rather than changing eviction order.

- [ ] **Step 5: Verify and commit**

Run `cargo test --lib storage::page storage::cache`, the full Clippy gate, and `cargo fmt --check`.

Expected: corrupted frames never enter the cache, cache length never exceeds capacity, pinned frames survive eviction, and all-pinned insertion returns `Busy`.

Commit: `git add src/storage/mod.rs src/storage/page.rs src/storage/cache.rs && git commit -m "feat: add validated page cache"`

### Task 4: Implement checked slotted pages and B+Tree cell codecs

**Files:**
- Create: `src/storage/slotted.rs`
- Create: `src/storage/btree/mod.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in both new files

**Interfaces:**
- Consumes: mutable private `PageFrame` values from Task 3.
- Produces: `SlottedPage::{open, cell, insert, remove, compact, contiguous_free, compacted_free}`.
- Produces: `LeafCell`, `InternalCell`, `ValueStorage`, `TreeKind`, and exact encode/decode methods.

- [ ] **Step 1: Write failing slot insertion/deletion/corruption tests**

```rust
#[test]
fn insert_remove_and_compact_preserve_cell_order() {
    let mut frame = leaf_frame(4096);
    let mut page = SlottedPage::open(&mut frame, 56).unwrap();
    page.insert(0, b"bbb").unwrap();
    page.insert(0, b"a").unwrap();
    page.insert(1, b"cc").unwrap();
    assert_eq!(page.cells().unwrap(), vec![b"a".to_vec(), b"cc".to_vec(), b"bbb".to_vec()]);
    assert_eq!(page.remove(1).unwrap(), b"cc");
    page.compact().unwrap();
    page.validate().unwrap();
}

#[test]
fn overlapping_slots_are_corruption() {
    let mut frame = encoded_frame_with_slots([(4000, 20), (4010, 20)]);
    assert_eq!(SlottedPage::open(&mut frame, 56).unwrap_err().code(), ErrorCode::Corruption);
}
```

Define `leaf_frame(page_size)` as a zeroed `TableLeaf` frame with PageId 1/generation 1. Define `encoded_frame_with_slots` by writing the supplied little-endian slot pairs after byte 56, updating the page-header layout, and stamping the whole-page checksum; it exists only in the `slotted.rs` test module.

- [ ] **Step 2: Implement the slotted-page algorithm**

Each slot is `(offset: u16, len: u16)` at `metadata_end + index * 4`. Validate checked additions, slot-directory end, payload bounds, pairwise non-overlap, and no overlap with header/metadata/slots. Insertion checks compacted capacity, compacts when contiguous space is insufficient, copies the cell downward from `payload_start`, shifts slot records, and updates header layout. Removal deletes the selected payload logically, shifts slots left, decrements the count, then compacts before returning so encoding is deterministic.

- [ ] **Step 3: Write failing leaf/internal codec tests**

```rust
#[test]
fn cell_codecs_round_trip_and_enforce_key_limit() {
    let leaf = LeafCell::inline(b"K".to_vec(), b"value".to_vec()).unwrap();
    assert_eq!(LeafCell::decode(&leaf.encode().unwrap()).unwrap(), leaf);
    let child = PageRef::new(PageId(9), 2).unwrap();
    let internal = InternalCell::new(b"M".to_vec(), child).unwrap();
    assert_eq!(InternalCell::decode(&internal.encode().unwrap()).unwrap(), internal);
    assert_eq!(LeafCell::inline(vec![0; 1025], vec![]).unwrap_err().code(), ErrorCode::Type);
}
```

- [ ] **Step 4: Implement exact node metadata and cell formats**

Use these fixed encodings:

```text
leaf metadata 48..56: right_sibling_generation u64 (zero iff no right sibling)
internal metadata 48..64: leftmost PageId u64, leftmost generation u64
leaf cell: key_len u16, storage u8 (0 inline, 1 overflow), reserved u8=0,
           total_value_len u64, key bytes, then inline bytes or PageId u64 + generation u64
internal cell: key_len u16, reserved u16=0, right PageId u64,
               right generation u64, separator key bytes
```

Reject trailing bytes, nonzero reserved fields, zero/invalid references, key length over 1024, total lengths that do not fit `u64`, and a minimum legal cell that cannot fit on the configured page.

- [ ] **Step 5: Verify and commit**

Run `cargo test --lib storage::slotted storage::btree`, `cargo fmt --check`, and the full Clippy gate.

Expected: all slot boundary, compaction, overlap, minimum-cell, and codec tests pass for all three page sizes.

Commit: `git add src/storage/mod.rs src/storage/slotted.rs src/storage/btree/mod.rs && git commit -m "feat: add slotted btree pages"`

### Task 5: Build Pager reads and private WriteBatch commit lifecycle

**Files:**
- Create: `src/storage/pager.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in `src/storage/pager.rs`

**Interfaces:**
- Consumes: `DatabaseFile`, `PageFrame`, `PageCache`, and header mutation APIs.
- Produces: `Pager::{create, open, read, begin_write, is_poisoned, page_size, page_count}`.
- Produces: `WriteBatch::{read, modify, allocate_append, release_later, set_catalog_root, commit, rollback}`.
- Produces internally: `PageSource::read_page(reference)` so validation and mutation can read either committed Pager pages or the batch overlay without weakening the public Phase 2 facade.

- [ ] **Step 1: Write failing read/cache/batch-exclusivity tests**

```rust
#[test]
fn pager_reads_validated_pages_and_keeps_dirty_images_private() {
    let temp = TempDatabase::new("pager-private");
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
    let root;
    {
        let mut batch = pager.begin_write().unwrap();
        root = batch.allocate_append(PageType::TableLeaf).unwrap();
        batch.modify(root).unwrap().borrow_mut().bytes_mut()[100] = 7;
        batch.rollback().unwrap();
    }
    assert_eq!(pager.read(root).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Define Pager state and read path**

Use these fields and signatures:

```rust
pub(super) struct PagerConfig {
    pub(super) page_size: u32,
    pub(super) uuid: [u8; 16],
    pub(super) max_database_bytes: u64,
    pub(super) cache_pages: usize,
}

impl PagerConfig {
    pub(super) fn new(page_size: u32, uuid: [u8; 16]) -> Result<Self>;
}

pub(super) enum PagerState { Ready, Poisoned }

pub(super) struct Pager {
    file: DatabaseFile,
    header: DatabaseHeader,
    cache: PageCache,
    state: PagerState,
}

pub(super) fn read(&mut self, reference: PageRef) -> Result<PageHandle>;
pub(super) fn begin_write(&mut self) -> Result<WriteBatch<'_>>;

pub(super) trait PageSource {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle>;
}
```

`read` checks Ready state, returns a matching-generation cached frame when present, otherwise reads and validates the complete page before cache insertion. No failed frame is inserted.

`PagerConfig::new` validates the page size and fills `max_database_bytes = DEFAULT_MAX_DATABASE_BYTES` and `cache_pages = 1024`. In `test_support.rs`, add `PagerConfig::test_default(page_size)` with a deterministic UUID and `test_pager(page_size) -> Result<(TempDatabase, Pager)>`; all later test snippets use that fixture. A Pager opened read-only permits reads and rejects `begin_write` with `Unsupported`.

- [ ] **Step 3: Write failing commit-order and poison tests**

Use a recording `FaultInjector` and assert the event sequence for one new page is `data write before/data write after`, sync, header write, sync. Inject failure at every before/after write/sync occurrence and assert `commit` returns `Io` and `pager.is_poisoned()` is true once file I/O has begun.

```rust
#[test]
fn logical_failure_before_io_can_roll_back_without_poisoning() {
    let mut pager = test_pager();
    let mut batch = pager.begin_write().unwrap();
    assert_eq!(batch.set_catalog_root(PageRef::new(PageId(999), 1).unwrap()).unwrap_err().code(), ErrorCode::Corruption);
    batch.rollback().unwrap();
    assert!(!pager.is_poisoned());
}
```

- [ ] **Step 4: Implement batch isolation and deterministic commit**

`WriteBatch<'a>` owns `&'a mut Pager`, `BTreeMap<PageId, PageFrame>` dirty images, `BTreeSet<PageId>` new pages, `BTreeMap<PageId, u64>` pending generations, `BTreeSet<PageId>` pending releases, and an optional candidate header. `modify` clones on first write. `allocate_append` checks max size and returns generation 1 without extending the file until commit. Implement `PageSource` for Pager and WriteBatch; the batch implementation checks its dirty overlay first. Dropping an uncommitted batch performs an in-memory rollback only.

`commit` performs: logical validation; file extension; ascending PageId writes after `prepare_for_write`; first sync; full zero-padded header-page write; second sync; cache replacement; state Ready. Mark the Pager Poisoned immediately before the first mutating file call and restore Ready only after the second sync succeeds. A poisoned Pager rejects all later reads and writes with `Io` context instructing reopen.

- [ ] **Step 5: Verify and commit**

Run `cargo test --lib storage::pager`, full Clippy, and formatting.

Expected: clean reads cache, dirty data is invisible outside its batch, rollback causes no file changes, commit order is deterministic, and all injected I/O failures poison the Pager.

Commit: `git add src/storage/mod.rs src/storage/pager.rs && git commit -m "feat: add pager write batches"`

### Task 6: Add SQLite-style freelist allocation and generation reuse

**Files:**
- Create: `src/storage/freelist.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in `src/storage/freelist.rs` and `src/storage/pager.rs`

**Interfaces:**
- Consumes: `WriteBatch` dirty/new/release collections from Task 5.
- Produces: `Freelist::{allocate, release, walk}` and integration through `WriteBatch::allocate`.
- Preserves: no page released in a batch can be allocated in the same batch.

- [ ] **Step 1: Write failing trunk transition and generation tests**

```rust
#[test]
fn full_root_promotes_released_page_and_empty_root_is_allocated() {
    let (_temp, mut pager) = test_pager(4096).unwrap();
    let capacity = Freelist::leaf_capacity(4096).unwrap();
    let refs = {
        let mut batch = pager.begin_write().unwrap();
        let refs = (0..=capacity)
            .map(|_| batch.allocate(PageType::TableLeaf).unwrap())
            .collect::<Vec<_>>();
        batch.commit().unwrap();
        refs
    };
    {
        let mut batch = pager.begin_write().unwrap();
        for reference in &refs { batch.release_later(*reference).unwrap(); }
        batch.commit().unwrap();
    }
    let mut next = pager.begin_write().unwrap();
    let reused = next.allocate(PageType::TableLeaf).unwrap();
    assert!(refs.iter().any(|old| old.page_id() == reused.page_id()));
    assert_eq!(reused.generation(), 2);
}
```

- [ ] **Step 2: Implement the trunk disk format**

At byte 48 encode next trunk PageId/generation (16 bytes), leaf count `u32` (4 bytes), reserved `u32=0`, then packed leaf PageIds as `u64`. A trunk's own generation comes from its page header; leaf generation is recovered from the released page header and incremented at allocation. Validate count capacity, zero/reserved fields, references, duplicates, cycles, and PageId 0.

- [ ] **Step 3: Integrate release and allocation rules**

At commit preparation, process pending releases in ascending PageId order. Push into the root trunk until full; when full, rewrite the released page as a new trunk pointing at the old root. Allocation first pops a leaf from the root; if there is no leaf, return the root trunk itself and advance the root. Only when the freelist is empty does allocation append at file tail. Clear the full page, set the requested PageType, and use `old_generation.checked_add(1)`; overflow is `Corruption`.

Before accepting a release, reject PageId 0, a current trunk, duplicate release, a dirty live page, the active catalog root, or any page marked reachable by B+Tree/overflow validation. Keep `released_this_batch` separate from the committed freelist so immediate reuse is impossible.

- [ ] **Step 4: Verify reuse and bounded growth**

Run a three-batch test: allocate and commit 100 pages; release and commit them; allocate and commit 100 pages. Assert generations increment, stale references fail, and final `page_count` is unchanged from the first allocation batch.

- [ ] **Step 5: Verify and commit**

Run `cargo test --lib storage::freelist storage::pager`, formatting, and full Clippy.

Expected: full/empty trunk transitions, deterministic ordering, stale-reference rejection, no same-batch reuse, and bounded file growth all pass.

Commit: `git add src/storage/mod.rs src/storage/freelist.rs src/storage/pager.rs && git commit -m "feat: add reusable page freelist"`

### Task 7: Implement overflow value chains

**Files:**
- Create: `src/storage/overflow.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in `src/storage/overflow.rs`

**Interfaces:**
- Consumes: batch page allocation/release and Pager reference-validated reads.
- Produces: `write_value(batch, bytes) -> Result<PageRef>`, `read_value(pager, first, total_len) -> Result<Vec<u8>>`, and `release_chain(batch, first, total_len)`.

- [ ] **Step 1: Write failing multi-page and corruption tests**

```rust
#[test]
fn overflow_round_trips_and_releases_multiple_pages() {
    let value = vec![0x6d; 9000];
    let (_temp, mut pager) = test_pager(4096).unwrap();
    let first = {
        let mut batch = pager.begin_write().unwrap();
        let first = write_value(&mut batch, &value).unwrap();
        batch.commit().unwrap();
        first
    };
    assert_eq!(read_value(&mut pager, first, value.len() as u64).unwrap(), value);
    let page_count = collect_chain(&mut pager, first, value.len() as u64).unwrap().len();
    assert_eq!(page_count, 3);
}
```

`collect_chain` is a test-visible form of the same bounded traversal used by `read_value`; it returns the visited `Vec<PageRef>` and performs every production validation before exposing the list.

Add separate tests that patch and restamp pages to create a cycle, wrong generation, wrong PageType, block length above capacity, early end, excess bytes, and a chain longer than ordinary page count; each must return `Corruption` without panic.

- [ ] **Step 2: Implement the overflow format and writer**

Use body bytes 48..64 for next PageId/generation, 64..68 for `chunk_len: u32`, 68..72 reserved zero, and 72.. for chunk bytes. Allocate enough pages using checked `usize` and `u64` arithmetic, write chunks in input order, and link each page to the next. Empty values remain inline and never allocate an overflow page.

- [ ] **Step 3: Implement bounded read and release traversal**

On every hop use `Pager::read(PageRef)`, require `PageType::Overflow`, insert PageId into a `BTreeSet`, reject repeats, and reject hops above `pager.page_count() - 1`. Check cumulative length before allocation/copy. The terminal next reference must be 0/0 and cumulative bytes must equal `total_len` exactly. `release_chain` performs the same traversal before adding every page to `pending_releases`.

- [ ] **Step 4: Verify and commit**

Run `cargo test --lib storage::overflow`, formatting, and full Clippy.

Expected: 0-, 1-, and multi-page boundaries pass at all page sizes; all malformed chains return `Corruption`; replacement releases the original chain for a future batch.

Commit: `git add src/storage/mod.rs src/storage/overflow.rs && git commit -m "feat: add overflow value chains"`

### Task 8: Implement B+Tree creation, point lookup, and streaming scans

**Files:**
- Modify: `src/storage/btree/mod.rs`
- Create: `src/storage/btree/search.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in both B+Tree files

**Interfaces:**
- Consumes: node codecs, Pager reads, WriteBatch allocation, and overflow reads.
- Produces: `BTree::{create, root, get, scan}` and `BTreeCursor::next`.
- Produces: exact child routing where separators equal the minimum key in their right subtree.

- [ ] **Step 1: Write failing create and routing tests**

```rust
#[test]
fn internal_routing_uses_last_separator_not_greater_than_key() {
    let node = internal_node(
        page_ref(1, 1),
        [(b"m", page_ref(2, 1)), (b"t", page_ref(3, 1))],
    );
    assert_eq!(node.child_for(b"a").unwrap(), page_ref(1, 1));
    assert_eq!(node.child_for(b"m").unwrap(), page_ref(2, 1));
    assert_eq!(node.child_for(b"z").unwrap(), page_ref(3, 1));
}

#[test]
fn empty_tree_is_one_leaf_and_scans_empty() {
    let (_temp, mut pager) = test_pager(4096).unwrap();
    let tree = {
        let mut batch = pager.begin_write().unwrap();
        let tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        batch.commit().unwrap();
        tree
    };
    assert_eq!(tree.scan(&mut pager, Bound::Unbounded, Bound::Unbounded).unwrap().next().unwrap(), None);
}
```

- [ ] **Step 2: Define the internal facade and node decoder**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeKind { Table, Index }

pub(super) struct BTree {
    kind: TreeKind,
    root: PageRef,
}

impl BTree {
    pub(super) fn create(batch: &mut WriteBatch<'_>, kind: TreeKind) -> Result<Self>;
    pub(super) const fn root(&self) -> PageRef;
    pub(super) fn get(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>>;
    pub(super) fn scan<'a>(&self, pager: &'a mut Pager, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<BTreeCursor<'a>>;
}
```

Map `TreeKind` and level to exact leaf/internal PageTypes. Decode every node through `SlottedPage`; require strict key ordering. Lookup rejects keys over 1024 as `Type`, descends with the defined separator rule, and binary-searches the leaf.

- [ ] **Step 3: Implement lower-bound positioning and cursor traversal**

`BTreeCursor<'a>` stores `&'a mut Pager`, current leaf `PageRef`, next slot, owned end bound, and a visited `BTreeSet<PageId>`. It loads one leaf at a time, decodes inline or overflow values, stops at the end bound, and follows the right sibling using PageId from the page header plus generation from leaf metadata. Reject sibling cycles, non-leaf targets, generation mismatches, and decreasing first keys.

- [ ] **Step 4: Verify and commit**

Run point/range tests for empty, one-leaf, and manually built three-level trees; include inclusive/exclusive/unbounded endpoints and an overflow value.

Run `cargo test --lib storage::btree::search`, formatting, and full Clippy.

Expected: exact routing, lexicographic keys, bounded cursor memory, and leaf-chain corruption tests pass.

Commit: `git add src/storage/mod.rs src/storage/btree/mod.rs src/storage/btree/search.rs && git commit -m "feat: add btree lookup and scans"`

### Task 9: Implement insert, replacement policy, and split propagation

**Files:**
- Create: `src/storage/btree/mutate.rs`
- Modify: `src/storage/btree/mod.rs`
- Test: unit tests in `src/storage/btree/mutate.rs`

**Interfaces:**
- Consumes: tree search paths, slotted pages, overflow chains, and batch allocation.
- Produces: `BTree::insert(&mut self, batch, key, value) -> Result<()>`.
- Produces: leaf/internal split results and root growth while maintaining exact separator minima.

- [ ] **Step 1: Write failing duplicate, large-value, and split tests**

```rust
#[test]
fn insert_rejects_duplicate_physical_key() {
    let (_temp, mut pager) = test_pager(4096).unwrap();
    let mut batch = pager.begin_write().unwrap();
    let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
    tree.insert(&mut batch, b"k", b"v1").unwrap();
    assert_eq!(tree.insert(&mut batch, b"k", b"v2").unwrap_err().code(), ErrorCode::Constraint);
}

#[test]
fn inserts_grow_root_and_keep_all_values_visible() {
    let (_temp, mut pager) = test_pager(4096).unwrap();
    let mut batch = pager.begin_write().unwrap();
    let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
    let old_root = tree.root();
    for i in 0..600u32 { tree.insert(&mut batch, &i.to_be_bytes(), &vec![i as u8; 40]).unwrap(); }
    assert_ne!(tree.root(), old_root);
    batch.commit().unwrap();
    assert_eq!(tree.get(&mut pager, &599u32.to_be_bytes()).unwrap().unwrap(), vec![599u32 as u8; 40]);
}
```

- [ ] **Step 2: Implement search paths and leaf insertion**

Record a path of `(parent PageRef, child_index)` while descending. Build the candidate `LeafCell` before mutating: inline only if the complete encoded cell fits the target after compaction, otherwise write the entire value to overflow pages. Duplicate keys return `Constraint` and release any newly allocated provisional overflow chain within the batch's in-memory allocation set. Add `track_tree(kind, root)` to `WriteBatch`; insertion refreshes the tracked root after any root change so Task 11 can add whole-tree commit validation without changing mutation APIs.

- [ ] **Step 3: Implement byte-balanced leaf splits**

Combine existing and new cells in sorted order, evaluate every legal boundary, and choose the boundary minimizing absolute encoded-byte difference; break ties on the lower split index. Allocate a right leaf, preserve the old sibling as its sibling, point left to right, rewrite both pages deterministically, and return `(right_first_key, right_ref)` to the parent.

- [ ] **Step 4: Implement internal splits and root growth**

Insert the separator after the split child. If an internal page overflows, choose a middle separator by encoded-byte balance, promote its key, use its right child as the new right page's leftmost child, and remove the promoted cell from both children. Propagate upward. When the old root splits, allocate a new internal root whose leftmost child is the old root and whose first cell points to the new right page; assign `self.root` only after all new structures validate.

Use a mutation guard that restores the prior in-memory tree root and dirty images for any logical error before commit I/O.

- [ ] **Step 5: Verify and commit**

Run deterministic ascending, descending, and shuffled insert tests at all page sizes. After every split, assert every key/value is retrievable, scan order matches input sort, separators match right minima, and overflow values round-trip.

Run `cargo test --lib storage::btree::mutate`, formatting, and full Clippy.

Expected: leaf splits, multi-level internal splits, root growth, duplicate errors, and maximum-key boundaries pass.

Commit: `git add src/storage/btree/mod.rs src/storage/btree/mutate.rs && git commit -m "feat: add btree insertion and splits"`

### Task 10: Implement delete, borrowing, merging, and root shrink

**Files:**
- Modify: `src/storage/btree/mutate.rs`
- Modify: `src/storage/btree/mod.rs`
- Test: unit tests in `src/storage/btree/mutate.rs`

**Interfaces:**
- Consumes: mutation paths and future-batch page release.
- Produces: `BTree::delete(&mut self, batch, key) -> Result<Option<Vec<u8>>>`.
- Preserves: 25% non-root occupancy except for a single legal oversized cell.

- [ ] **Step 1: Write failing delete and root-shrink tests**

```rust
#[test]
fn delete_returns_value_and_missing_key_is_noop() {
    let (_temp, mut pager, mut tree) = committed_populated_tree(4096, 300);
    let mut batch = pager.begin_write().unwrap();
    assert_eq!(tree.delete(&mut batch, b"missing").unwrap(), None);
    assert_eq!(tree.delete(&mut batch, &key(150)).unwrap(), Some(value(150)));
    batch.commit().unwrap();
    assert_eq!(tree.get(&mut pager, &key(150)).unwrap(), None);
}

#[test]
fn deleting_to_empty_shrinks_to_one_leaf_root() {
    let (mut tree, mut batch) = populated_tree(4096, 600);
    for i in 0..600 { tree.delete(&mut batch, key(i)).unwrap(); }
    assert!(root_is_empty_leaf(&mut batch, tree.root()));
}
```

- [ ] **Step 2: Implement leaf deletion and overflow release**

Locate the key with its full parent path. Decode the value before removing it, traverse and queue any overflow chain for release, remove the cell, and update ancestor separators whenever a right child's minimum key changes. Return the removed bytes; a missing key performs no page mutation.

- [ ] **Step 3: Implement deterministic borrow rules**

Compute occupancy as encoded cell bytes divided by usable bytes after node metadata. If a non-root page is below 25%, inspect left sibling then right sibling under the same parent. Borrow the smallest number of boundary cells that raises the receiver to at least 25% while leaving the donor at least 25%; prefer left on equal movement cost. For internal nodes, rotate the parent separator and child pointer according to the exact minimum-of-right-subtree invariant, then recompute affected separator keys from child minima.

- [ ] **Step 4: Implement merges and recursive parent repair**

If no legal borrow exists, prefer merge into the left sibling; otherwise merge the right sibling into the page. Merge only when all encoded cells and metadata fit after compaction. Fix the leaf sibling link, remove the matching parent separator, and queue the discarded page for future-batch reuse. Apply the same process recursively to an underfull parent.

If an internal root has one child, make that child the new root and release the old root. If deletion empties a leaf root, retain it. Treat a single legal cell exceeding the 25% threshold calculation as exempt, not corrupt.

- [ ] **Step 5: Verify and commit**

Create focused layouts that force left borrow, right borrow, leaf merge, internal merge, cascading merge, and root shrink at each page size. Assert returned values, scan order, separators, sibling chain, delayed reuse, and stale-generation rejection.

Run `cargo test --lib storage::btree::mutate`, formatting, and full Clippy.

Expected: every rebalance path passes without loading the full tree into memory.

Commit: `git add src/storage/btree/mod.rs src/storage/btree/mutate.rs && git commit -m "feat: add btree deletion and rebalance"`

### Task 11: Add whole-structure validation and catalog-root persistence

**Files:**
- Create: `src/storage/validate.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/btree/mod.rs`
- Modify: `src/storage/mod.rs`
- Test: unit tests in `src/storage/validate.rs` and `src/storage/pager.rs`

**Interfaces:**
- Consumes: all disk codecs and Pager read APIs.
- Produces: `validate_tree(source: &mut impl PageSource, tree: &BTree) -> Result<ValidationReport>` and `validate_freelist(source: &mut impl PageSource, excluded: &BTreeSet<PageId>) -> Result<BTreeSet<PageId>>`.
- Produces: `WriteBatch::set_catalog_root(root: PageRef) -> Result<()>` and `Pager::catalog_tree(kind) -> Result<Option<BTree>>`.

- [ ] **Step 1: Write failing validator corruption tests**

Build a valid three-level tree with overflow values and freed pages, then patch and restamp one condition per test: duplicate reachable child, unequal leaf depth, bad separator minimum, unsorted page, sibling cycle, missing leaf from sibling chain, overflow/freelist overlap, duplicate freelist leaf, and out-of-range reference. Every case must return `Corruption`.

```rust
#[test]
fn valid_tree_reports_disjoint_page_sets() {
    let report = validate_tree(&mut pager, &tree).unwrap();
    assert!(report.tree_pages.is_disjoint(&report.overflow_pages));
    let free = validate_freelist(&mut pager, &report.all_live_pages()).unwrap();
    assert!(free.is_disjoint(&report.all_live_pages()));
}
```

- [ ] **Step 2: Implement a bounded recursive structural walk**

Return a summary per subtree containing minimum/maximum key, leaf depth, ordered leaf refs, tree-page set, and overflow-page set. Accept any `&mut impl PageSource` so the same traversal sees either committed Pager pages or a WriteBatch overlay. Reject duplicate PageIds before descending. Validate PageType against kind/level, strict per-page keys, parent separators equal right minima, uniform leaf depth, complete ordered sibling traversal, every slot range, every overflow chain, and all reference generations.

Then walk the freelist with separate trunk/leaf sets and reject cycles, duplicates, PageId 0, out-of-range pages, non-freelist trunks, and intersection with live tree/overflow pages. Any state that cannot be proven valid returns `Corruption`.

Wire every `(TreeKind, PageRef)` registered through `WriteBatch::track_tree` into commit preflight. Validate those trees and the candidate freelist against the batch overlay before the first file mutation; this upgrades Task 5's page-local commit checks to the full affected-tree check required by the specification.

- [ ] **Step 3: Implement safe catalog-root publication and recovery**

`set_catalog_root(PageRef)` infers `TreeKind` from the root PageType, runs validation through the batch's `PageSource` overlay, requires the reference to be the unique tree root, records only its PageId in the candidate header, and protects the root from release/reuse until the header's final sync. `Pager::catalog_tree(kind)` reads the root page named by the header, recovers generation from its validated PageHeader, checks root PageType against kind, constructs `BTree`, and validates it before returning.

- [ ] **Step 4: Write and pass reopen persistence tests**

For each page size: create a tree, insert enough mixed inline/overflow values to reach three levels, delete a deterministic subset, call `set_catalog_root`, commit, drop Pager, reopen, reconstruct from `catalog_root`, validate, and compare complete scan output. Open/close a second time without writes and assert file bytes are unchanged.

- [ ] **Step 5: Verify and commit**

Run `cargo test --lib storage::validate storage::pager`, formatting, and full Clippy.

Expected: valid structures report disjoint sets, every injected structural defect returns `Corruption`, catalog root survives reopen, and read-only reopen performs no writes.

Commit: `git add src/storage/mod.rs src/storage/validate.rs src/storage/pager.rs src/storage/btree/mod.rs && git commit -m "feat: validate and reopen persisted trees"`

### Task 12: Add deterministic model, fault, corruption, and scale gates

**Files:**
- Create: `src/storage/test_support.rs`
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/btree/mod.rs`
- Modify: `README.md`
- Test: integration-style crate-private tests in `src/storage/test_support.rs`

**Interfaces:**
- Consumes: the complete Phase 2 storage engine.
- Produces: fixed-seed `BTreeMap<Vec<u8>, Vec<u8>>` model comparison across every supported page size.
- Produces: manual ignored million-record scale probe with reported file size, height, reuse, and elapsed time.

- [ ] **Step 1: Implement deterministic test infrastructure**

Use a dependency-free xorshift generator with a fixed nonzero seed:

```rust
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
```

Temporary filenames combine process ID, test label, and an atomic counter. A Drop guard removes only the exact generated file. The recording fault injector fails on a configured `(FaultPoint, occurrence)` and records every observed event.

- [ ] **Step 2: Add the reference-model workload**

For each page size and both `TreeKind::Table` and `TreeKind::Index`, run 10,000 fixed-seed operations over keys of lengths 0..64 and values spanning inline/overflow thresholds: 45% insert, 25% delete, 20% get, 10% range scan. Mirror successful operations in `std::collections::BTreeMap`; duplicate insert must return `Constraint`. Compare every operation result, run `validate_tree` every 100 operations, commit/reopen every 1,000, and compare a full scan after each reopen.

- [ ] **Step 3: Add final fault and byte-corruption matrices**

For every file write/sync occurrence in a multi-page commit, inject before and after failure, assert the error is `Io`, and assert all later operations on that Pager fail until reopen. Independently truncate at header, partial-page, and page-boundary offsets; flip one byte in the header and each ordinary page type; patch/restamp PageId, generation, slot, sibling, child, overflow, and freelist fields. Assert every open/read/validate path returns a controlled error and never panics.

- [ ] **Step 4: Add the ignored scale probe**

```rust
#[test]
#[ignore = "manual million-record Phase 2 scale probe"]
fn million_small_records_report_storage_shape() {
    let metrics = run_scale_probe(1_000_000, 8192).expect("scale probe");
    eprintln!("rows={} bytes={} height={} reused_pages={} elapsed_ms={}",
        metrics.rows, metrics.file_bytes, metrics.tree_height,
        metrics.reused_pages, metrics.elapsed.as_millis());
    assert_eq!(metrics.rows, 1_000_000);
}
```

The regular CI suite does not run ignored tests. The formal 10-million-row/10-GiB acceptance remains Phase 6.

- [ ] **Step 5: Update the README truthfully**

State that Phase 2 implements an internal single-file Pager and full-mutation B+Tree with bounded cache, checksums, generations, overflow, freelist, validation, and normal-close persistence. Add this explicit warning: “Phase 2 has no WAL. A process or machine failure during a multi-page commit can leave a partially updated file; only successful commit followed by normal close/reopen is guaranteed.” Keep SQL and public database CRUD API listed as unimplemented.

- [ ] **Step 6: Rebase on merged CI foundation and run the completion gate**

After PR #2 is merged, update the Phase 2 branch from `origin/main` without mixing CI workflow edits into Phase 2 commits. Resolve only genuine overlap.

Run:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
git diff --check
```

Then confirm the GitHub checks for Linux stable, Linux Rust 1.85, and Windows stable all pass.

Expected: all local commands exit 0; model, persistence, corruption, cache, fault, and mutation tests pass for 4096/8192/16384; the ignored scale probe is compiled but not executed.

- [ ] **Step 7: Commit the phase gate**

```powershell
git add src/storage/mod.rs src/storage/test_support.rs src/storage/pager.rs src/storage/btree/mod.rs README.md
git commit -m "test: complete phase 2 storage gates"
```

Expected: `git status --short --branch` is clean and no Phase 3 WAL, SQL, public database API, concurrency, or file-locking code is present.
