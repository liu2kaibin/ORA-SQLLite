//! A deliberately small, read-only format-2 witness for the scale gate.
//!
//! It is not a second storage engine: it only decodes the superblock, the
//! catalog's explicit-index definitions, and B+Tree page framing needed to
//! prove that physical index keys were persisted.  All ranges, page references,
//! slot layouts, checksums, tree ordering, and leaf links are checked before an
//! entry is accepted.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
};

use ora_sqllite::{Database, Options, Parameters};

use super::TestFiles;

const SLOT_LEN: usize = 4;
const SUPERBLOCK_LEN: usize = 4096;
const SUPERBLOCK_AREA_LEN: usize = SUPERBLOCK_LEN * 2;
const PAGE_HEADER_LEN: usize = 48;
const LEAF_METADATA_END: usize = 56;
const INTERNAL_METADATA_END: usize = 64;
const FEATURE_CATALOG_V1: u64 = 1;
const FEATURE_INDEX_V1: u64 = 2;

#[derive(Debug, Clone)]
pub struct IndexVerification {
    pub index_page_ids: Vec<u64>,
}

/// A raw page identity captured by the test-only format-2 witness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawPageRef {
    pub id: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawColumn {
    pub id: u64,
    pub ordinal: u16,
    pub nullable: bool,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawTable {
    pub id: u64,
    pub root: RawPageRef,
    /// Physical type of the root page in this checkpointed snapshot.
    pub root_page_type: u8,
    pub next_rowid: u64,
    pub columns: Vec<RawColumn>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIndex {
    pub id: u64,
    pub table_id: u64,
    pub root: RawPageRef,
    /// Physical type of the root page in this checkpointed snapshot.
    pub root_page_type: u8,
    pub unique: bool,
    pub owner_constraint_id: Option<u64>,
    pub columns: Vec<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawConstraint {
    pub id: u64,
    pub table_id: u64,
    pub kind: u8,
    pub index_id: Option<u64>,
    pub columns: Vec<u64>,
    pub check_program_len: u32,
    pub check_program_version: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawFreelistTrunk {
    pub reference: RawPageRef,
    pub leaves: Vec<u64>,
}

/// A direct byte-level catalog snapshot.  This deliberately has no dependency
/// on `ConstraintDef` or `CheckProgram`: all fields below are decoded from
/// record envelopes and payload offsets owned by this test-only witness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawCatalogCapture {
    pub feature_bits: u64,
    pub catalog_root: RawPageRef,
    /// The accepted LSN once the caller has checkpointed the target snapshot.
    pub accepted_lsn: u64,
    pub next_object_id: u64,
    pub page_count: u64,
    pub freelist: Vec<RawFreelistTrunk>,
    pub tables: BTreeMap<String, RawTable>,
    pub indexes: BTreeMap<String, RawIndex>,
    pub constraints: BTreeMap<String, RawConstraint>,
    pub table_constraints: BTreeMap<u64, Vec<u64>>,
}

/// Creates a committed format-2 catalog that predates the local-constraint
/// extension.  The fixture uses only base catalog records and checkpoints them
/// so byte-level witnesses never need a production catalog decoder or WAL.
pub fn legacy_catalog_fixture() -> TestFiles {
    let files = TestFiles::new("format2-legacy-catalog");
    let database =
        Database::create(files.main(), Options::default()).expect("create legacy format-2 fixture");
    let mut connection = database.connect().expect("connect legacy fixture");
    let create = connection
        .prepare("CREATE TABLE LEGACY (ID INT64, NOTE TEXT)")
        .expect("prepare legacy table");
    connection
        .execute(&create, Parameters::none())
        .expect("create legacy table");
    let insert = connection
        .prepare("INSERT INTO LEGACY VALUES (1, 'before-open')")
        .expect("prepare legacy seed row");
    connection
        .execute(&insert, Parameters::none())
        .expect("insert legacy seed row");
    drop(connection);
    drop(database);
    super::checkpoint_accepted_state(&files);
    files
}

/// Reads the accepted format-2 catalog tree without invoking production
/// catalog-record decoders.  The sorted, unique families are sufficient to
/// prove a legacy file remained a base-catalog image after ordinary CRUD.
pub fn catalog_record_families(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|error| format!("read main file: {error}"))?;
    let header = select_header(&bytes)?;
    let disk = Disk {
        bytes: &bytes,
        header,
    };
    let catalog = disk.walk_tree(
        PageRef {
            id: disk.header.catalog_root,
            generation: disk.page_generation(disk.header.catalog_root)?,
        },
        TreeKind::Table,
        false,
    )?;
    let mut families = BTreeSet::new();
    for (key, _) in catalog.entries {
        families.insert(*key.first().ok_or("catalog contains an empty key")?);
    }
    Ok(families.into_iter().collect())
}

/// Captures a checkpointed format-2 catalog directly from the main file.
/// Callers intentionally checkpoint first: this witness does not reuse the
/// production WAL, catalog, constraint, or check-program decoders.
pub fn raw_catalog_capture(path: &std::path::Path) -> Result<RawCatalogCapture, String> {
    let bytes = fs::read(path).map_err(|error| format!("read main file: {error}"))?;
    let header = select_header(&bytes)?;
    let disk = Disk {
        bytes: &bytes,
        header,
    };
    let catalog_root = RawPageRef {
        id: disk.header.catalog_root,
        generation: disk.page_generation(disk.header.catalog_root)?,
    };
    let catalog = disk.walk_tree(
        PageRef {
            id: catalog_root.id,
            generation: catalog_root.generation,
        },
        TreeKind::Table,
        false,
    )?;
    let mut parsed = parse_raw_catalog(&catalog.entries)?;
    for table in parsed.tables.values_mut() {
        table.root_page_type = disk
            .page(table.root.id, Some(table.root.generation))?
            .page_type;
    }
    for index in parsed.indexes.values_mut() {
        index.root_page_type = disk
            .page(index.root.id, Some(index.root.generation))?
            .page_type;
    }
    Ok(RawCatalogCapture {
        feature_bits: disk.header.features,
        catalog_root,
        accepted_lsn: disk.header.checkpoint_lsn,
        next_object_id: parsed.next_object_id,
        page_count: disk.header.page_count,
        freelist: raw_freelist(&disk)?,
        tables: parsed.tables,
        indexes: parsed.indexes,
        constraints: parsed.constraints,
        table_constraints: parsed.table_constraints,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageRef {
    id: u64,
    generation: u64,
}

#[derive(Clone, Debug)]
struct Header {
    page_size: usize,
    publish_sequence: u64,
    checkpoint_lsn: u64,
    page_count: u64,
    catalog_root: u64,
    freelist_root: u64,
    features: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeKind {
    Table,
    Index,
}

impl TreeKind {
    fn leaf_type(self) -> u8 {
        match self {
            Self::Table => 3,
            Self::Index => 5,
        }
    }

    fn internal_type(self) -> u8 {
        match self {
            Self::Table => 2,
            Self::Index => 4,
        }
    }
}

#[derive(Debug)]
struct Page<'a> {
    page_type: u8,
    id: u64,
    generation: u64,
    right_sibling: Option<u64>,
    metadata: &'a [u8],
    cells: Vec<&'a [u8]>,
}

#[derive(Debug)]
struct TreeScan {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pages: Vec<u64>,
}

#[derive(Debug)]
struct SubtreeSummary {
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    depth: usize,
}

#[derive(Debug)]
struct ExplicitIndexDefinition {
    root: PageRef,
    unique: bool,
    columns: Vec<String>,
}

#[derive(Debug)]
struct CatalogColumn {
    table_id: u64,
    name: String,
}

pub fn assert_index_model(files: &TestFiles, expected_rows: usize, names: &[&str]) {
    verify_index_model(files, expected_rows, names).unwrap_or_else(|error| {
        panic!("format-2 physical index verification failed: {error}");
    });
}

pub fn verify_index_model(
    files: &TestFiles,
    expected_rows: usize,
    names: &[&str],
) -> Result<IndexVerification, String> {
    let bytes = fs::read(files.main()).map_err(|error| format!("read main file: {error}"))?;
    let header = select_header(&bytes)?;
    if header.features & (FEATURE_CATALOG_V1 | FEATURE_INDEX_V1)
        != FEATURE_CATALOG_V1 | FEATURE_INDEX_V1
    {
        return Err("accepted image does not advertise catalog and index V1".into());
    }
    let disk = Disk {
        bytes: &bytes,
        header,
    };
    let catalog = disk.walk_tree(
        PageRef {
            id: disk.header.catalog_root,
            generation: disk.page_generation(disk.header.catalog_root)?,
        },
        TreeKind::Table,
        false,
    )?;
    let definitions = explicit_index_definitions(&catalog.entries)?;
    if definitions.len() != names.len() {
        return Err(format!(
            "catalog has {} explicit indexes, expected {}",
            definitions.len(),
            names.len()
        ));
    }

    let mut all_pages = catalog.pages.into_iter().collect::<HashSet<_>>();
    let mut index_page_ids = Vec::new();
    for &name in names {
        let definition = definitions
            .get(name)
            .ok_or_else(|| format!("catalog misses expected index {name}"))?;
        assert_expected_index_metadata(name, definition)?;
        let scan = disk.walk_tree(definition.root, TreeKind::Index, true)?;
        if scan.entries.len() != expected_rows {
            return Err(format!(
                "{name} has {} physical entries, expected {expected_rows}",
                scan.entries.len()
            ));
        }
        for page in &scan.pages {
            if !all_pages.insert(*page) {
                return Err(format!(
                    "{name} overlaps a catalog or sibling index page {page}"
                ));
            }
        }
        index_page_ids.extend(scan.pages.iter().copied());
        if expected_rows == 99_000 {
            assert_selected_physical_pairs(name, &scan.entries)?;
        }
    }
    index_page_ids.sort_unstable();
    Ok(IndexVerification { index_page_ids })
}

pub fn flip_physical_page_byte(files: &TestFiles, page_id: u64, page_offset: u64) {
    let bytes = fs::read(files.main()).expect("read database before physical corruption");
    let header = select_header(&bytes).expect("parse database before physical corruption");
    let start = ordinary_page_offset(page_id, header.page_size).expect("valid page offset");
    let page_offset = usize::try_from(page_offset).expect("physical corruption offset fits");
    let position = start
        .checked_add(page_offset)
        .expect("physical corruption offset fits");
    assert!(
        position < bytes.len(),
        "physical corruption byte is in the page"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(files.main())
        .expect("open database for focused verifier corruption");
    file.seek(SeekFrom::Start(
        u64::try_from(position).expect("physical corruption seek position fits"),
    ))
    .expect("seek physical corruption byte");
    file.write_all(&[bytes[position] ^ 1])
        .expect("write physical corruption byte");
    file.sync_all().expect("sync physical corruption byte");
}

pub fn corrupt_first_index_separator_with_valid_checksum(
    files: &TestFiles,
    index_name: &str,
) -> Result<(), String> {
    let mut bytes = fs::read(files.main()).map_err(|error| format!("read database: {error}"))?;
    let header = select_header(&bytes)?;
    let (page_size, root) = {
        let disk = Disk {
            bytes: &bytes,
            header,
        };
        let catalog = disk.walk_tree(
            PageRef {
                id: disk.header.catalog_root,
                generation: disk.page_generation(disk.header.catalog_root)?,
            },
            TreeKind::Table,
            false,
        )?;
        let root = explicit_index_definitions(&catalog.entries)?
            .get(index_name)
            .ok_or_else(|| format!("catalog misses target index {index_name}"))?
            .root;
        (disk.header.page_size, root)
    };
    let start = ordinary_page_offset(root.id, page_size)?;
    let end = start
        .checked_add(page_size)
        .ok_or("target index root extent overflows")?;
    let page = bytes
        .get_mut(start..end)
        .ok_or("target index root lies outside the database image")?;
    if page.first() != Some(&TreeKind::Index.internal_type()) {
        return Err("target index root is not internal".into());
    }
    let first_cell = usize::from(read_u16(page, INTERNAL_METADATA_END)?);
    let key_len = usize::from(read_u16(page, first_cell)?);
    let key_start = first_cell
        .checked_add(20)
        .ok_or("first separator key offset overflows")?;
    let key_end = key_start
        .checked_add(key_len)
        .ok_or("first separator key extent overflows")?;
    let key = page
        .get_mut(key_start..key_end)
        .ok_or("first separator key lies outside the root page")?;
    let byte = key
        .iter_mut()
        .rev()
        .find(|byte| **byte != 0)
        .ok_or("first separator key is all zero")?;
    *byte = byte
        .checked_sub(1)
        .ok_or("first separator key cannot be decremented")?;
    let checksum = crc32c_parts(
        page.get(..36)
            .ok_or("target index root lacks checksum prefix")?,
        page.get(40..)
            .ok_or("target index root lacks checksum suffix")?,
    );
    page.get_mut(36..40)
        .ok_or("target index root lacks checksum field")?
        .copy_from_slice(&checksum.to_le_bytes());
    fs::write(files.main(), bytes).map_err(|error| format!("write database: {error}"))?;
    Ok(())
}

struct Disk<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl Disk<'_> {
    fn page_generation(&self, id: u64) -> Result<u64, String> {
        Ok(self.page(id, None)?.generation)
    }

    fn page(&self, id: u64, expected_generation: Option<u64>) -> Result<Page<'_>, String> {
        let start = ordinary_page_offset(id, self.header.page_size)?;
        let end = start
            .checked_add(self.header.page_size)
            .ok_or("page extent overflow")?;
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or("page lies outside accepted file")?;
        verify_page_checksum(bytes)?;
        if bytes.len() != self.header.page_size || bytes.len() < PAGE_HEADER_LEN {
            return Err("invalid page size".into());
        }
        let page_type = byte(bytes, 0)?;
        if !(1..=7).contains(&page_type) {
            return Err("unknown page type".into());
        }
        if bytes[1] != 0
            || read_u16(bytes, 2)? != 0
            || read_u16(bytes, 26)? != 0
            || bytes[40..48].iter().any(|byte| *byte != 0)
        {
            return Err("page header reserved bytes are nonzero".into());
        }
        let page_id = read_u64(bytes, 4)?;
        let generation = read_u64(bytes, 12)?;
        if page_id != id || generation == 0 {
            return Err("page identity or generation is invalid".into());
        }
        if expected_generation.is_some_and(|expected| expected != generation) {
            return Err("page reference generation does not match disk".into());
        }
        let cell_count = usize::from(read_u16(bytes, 20)?);
        let slot_end = usize::from(read_u16(bytes, 22)?);
        let payload_start = usize::from(read_u16(bytes, 24)?);
        let right_sibling = match read_u64(bytes, 28)? {
            0 => None,
            sibling if sibling < self.header.page_count => Some(sibling),
            _ => return Err("right sibling lies outside accepted page range".into()),
        };
        let metadata_end = match page_type {
            2 | 4 => INTERNAL_METADATA_END,
            3 | 5 => LEAF_METADATA_END,
            _ => return Err("physical verifier reached a non-B+Tree page".into()),
        };
        let expected_slot_end = metadata_end
            .checked_add(
                cell_count
                    .checked_mul(SLOT_LEN)
                    .ok_or("slot count overflow")?,
            )
            .ok_or("slot directory overflow")?;
        if slot_end != expected_slot_end || slot_end > payload_start || payload_start > bytes.len()
        {
            return Err("slotted page layout is inconsistent".into());
        }
        let mut cells = Vec::with_capacity(cell_count);
        let mut ranges = Vec::with_capacity(cell_count);
        for slot in 0..cell_count {
            let slot_start = metadata_end + slot * SLOT_LEN;
            let offset = usize::from(read_u16(bytes, slot_start)?);
            let len = usize::from(read_u16(bytes, slot_start + 2)?);
            let end = offset.checked_add(len).ok_or("cell extent overflow")?;
            if offset < slot_end || offset < payload_start || end > bytes.len() {
                return Err("cell lies outside the slotted payload area".into());
            }
            if ranges
                .iter()
                .any(|&(prior_start, prior_end)| offset < prior_end && prior_start < end)
            {
                return Err("cell payload slots overlap".into());
            }
            ranges.push((offset, end));
            cells.push(&bytes[offset..end]);
        }
        let lowest = ranges
            .iter()
            .map(|(start, _)| *start)
            .min()
            .unwrap_or(bytes.len());
        if lowest != payload_start {
            return Err("payload start does not name the lowest live cell".into());
        }
        Ok(Page {
            page_type,
            id: page_id,
            generation,
            right_sibling,
            metadata: &bytes[PAGE_HEADER_LEN..metadata_end],
            cells,
        })
    }

    fn walk_tree(
        &self,
        root: PageRef,
        kind: TreeKind,
        require_empty_values: bool,
    ) -> Result<TreeScan, String> {
        if root.id == 0 || root.id >= self.header.page_count || root.generation == 0 {
            return Err("tree root is outside accepted page range".into());
        }
        let mut visited = HashSet::new();
        let mut pages = Vec::new();
        let mut leaves = Vec::new();
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let page_limit =
            usize::try_from(self.header.page_count).map_err(|_| "page count exceeds platform")?;
        self.walk_subtree(
            root,
            kind,
            require_empty_values,
            true,
            page_limit,
            &mut visited,
            &mut pages,
            &mut leaves,
            &mut entries,
        )?;
        for (position, (_, sibling)) in leaves.iter().enumerate() {
            let expected = leaves.get(position + 1).map(|(reference, _)| *reference);
            if *sibling != expected {
                return Err("B+Tree leaf sibling chain disagrees with tree traversal".into());
            }
        }
        Ok(TreeScan { entries, pages })
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_subtree(
        &self,
        reference: PageRef,
        kind: TreeKind,
        require_empty_values: bool,
        is_root: bool,
        page_limit: usize,
        visited: &mut HashSet<u64>,
        pages: &mut Vec<u64>,
        leaves: &mut Vec<(PageRef, Option<PageRef>)>,
        entries: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<Option<SubtreeSummary>, String> {
        if !visited.insert(reference.id) || visited.len() >= page_limit {
            return Err("B+Tree contains a cycle or too many pages".into());
        }
        let page = self.page(reference.id, Some(reference.generation))?;
        pages.push(page.id);
        match page.page_type {
            value if value == kind.leaf_type() => {
                if !is_root && page.cells.is_empty() {
                    return Err("non-root B+Tree leaf is empty".into());
                }
                let sibling = decode_leaf_sibling(&page, self.header.page_count)?;
                let mut min_key = None;
                let mut max_key = None;
                for cell in page.cells {
                    let (key, value) = decode_leaf_cell(cell)?;
                    if require_empty_values && !value.is_empty() {
                        return Err("physical index leaf stores a nonempty value".into());
                    }
                    if max_key
                        .as_ref()
                        .is_some_and(|prior: &Vec<u8>| key.as_slice() <= prior.as_slice())
                    {
                        return Err("B+Tree leaf keys are not strictly ordered".into());
                    }
                    if entries
                        .last()
                        .is_some_and(|(prior, _)| key.as_slice() <= prior.as_slice())
                    {
                        return Err("B+Tree leaf keys are not strictly ordered".into());
                    }
                    if min_key.is_none() {
                        min_key = Some(key.clone());
                    }
                    max_key = Some(key.clone());
                    entries.push((key, value));
                }
                leaves.push((reference, sibling));
                Ok(match (min_key, max_key) {
                    (Some(min_key), Some(max_key)) => Some(SubtreeSummary {
                        min_key,
                        max_key,
                        depth: 0,
                    }),
                    (None, None) if is_root => None,
                    _ => return Err("B+Tree leaf range is inconsistent".into()),
                })
            }
            value if value == kind.internal_type() => {
                let (children, keys) = decode_internal_children(&page, self.header.page_count)?;
                if keys.is_empty() {
                    return Err("B+Tree internal node is empty".into());
                }
                if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return Err("B+Tree internal keys are not strictly ordered".into());
                }
                let mut summaries = Vec::with_capacity(children.len());
                for child in children {
                    summaries.push(
                        self.walk_subtree(
                            child,
                            kind,
                            require_empty_values,
                            false,
                            page_limit,
                            visited,
                            pages,
                            leaves,
                            entries,
                        )?
                        .ok_or("non-root B+Tree child is empty")?,
                    );
                }
                let depth = summaries[0].depth;
                if summaries.iter().any(|summary| summary.depth != depth) {
                    return Err("B+Tree leaves do not have equal depth".into());
                }
                for (position, right) in summaries.iter().enumerate().skip(1) {
                    let left = &summaries[position - 1];
                    if left.max_key >= right.min_key {
                        return Err("B+Tree child key ranges overlap".into());
                    }
                    if keys[position - 1] != right.min_key {
                        return Err(
                            "B+Tree separator does not equal its right-child minimum".into()
                        );
                    }
                }
                Ok(Some(SubtreeSummary {
                    min_key: summaries[0].min_key.clone(),
                    max_key: summaries
                        .last()
                        .expect("nonempty internal node has a final child")
                        .max_key
                        .clone(),
                    depth: depth.checked_add(1).ok_or("B+Tree depth overflow")?,
                }))
            }
            _ => Err("B+Tree page kind changes below its root".into()),
        }
    }
}

fn explicit_index_definitions(
    entries: &[(Vec<u8>, Vec<u8>)],
) -> Result<BTreeMap<String, ExplicitIndexDefinition>, String> {
    struct PendingDefinition {
        id: u64,
        table_id: u64,
        root: PageRef,
        unique: bool,
        column_count: u16,
    }

    let mut definitions = BTreeMap::new();
    let mut mappings = BTreeMap::<u64, BTreeMap<u16, u64>>::new();
    let mut columns = BTreeMap::<u64, CatalogColumn>::new();
    for (key, value) in entries {
        match key.first() {
            Some(0x21) => {
                if key.len() != 9 {
                    return Err("index definition key is malformed".into());
                }
                let payload = catalog_payload(value, 0x21, "index catalog record")?;
                if payload.len() < 48
                    || payload[35] != 0
                    || read_u16(payload, 36)? != 0
                    || read_u64(payload, 38)? != 0
                {
                    return Err("explicit index definition is malformed".into());
                }
                let id = read_u64(payload, 0)?;
                let table_id = read_u64(payload, 8)?;
                if id == 0 || key[1..9] != id.to_be_bytes() || table_id == 0 {
                    return Err("index definition identity disagrees with its catalog key".into());
                }
                let root = PageRef {
                    id: read_u64(payload, 16)?,
                    generation: read_u64(payload, 24)?,
                };
                let column_count = read_u16(payload, 32)?;
                if root.id == 0 || root.generation == 0 || column_count == 0 || payload[34] > 1 {
                    return Err("index definition fields are invalid".into());
                }
                let name = catalog_name(payload, 46, 48, "index")?;
                if definitions
                    .insert(
                        name,
                        PendingDefinition {
                            id,
                            table_id,
                            root,
                            unique: payload[34] == 1,
                            column_count,
                        },
                    )
                    .is_some()
                {
                    return Err("catalog has duplicate explicit index names".into());
                }
            }
            Some(0x23) => {
                if key.len() != 11 {
                    return Err("index-column mapping key is malformed".into());
                }
                let index_id = u64::from_be_bytes(
                    key[1..9]
                        .try_into()
                        .map_err(|_| "index-column mapping key is truncated")?,
                );
                let ordinal = u16::from_be_bytes(
                    key[9..11]
                        .try_into()
                        .map_err(|_| "index-column mapping ordinal is truncated")?,
                );
                let payload = catalog_payload(value, 0x23, "index-column mapping")?;
                if payload.len() != 8 {
                    return Err("index-column mapping payload is malformed".into());
                }
                let column_id = read_u64(payload, 0)?;
                if index_id == 0 || column_id == 0 {
                    return Err("index-column mapping identity is invalid".into());
                }
                if mappings
                    .entry(index_id)
                    .or_default()
                    .insert(ordinal, column_id)
                    .is_some()
                {
                    return Err("catalog has duplicate index-column mappings".into());
                }
            }
            Some(0x14) => {
                if key.len() != 9 {
                    return Err("column definition key is malformed".into());
                }
                let payload = catalog_payload(value, 0x14, "column definition")?;
                if payload.len() < 24 || read_u16(payload, 22)? != 0 {
                    return Err("column definition payload is malformed".into());
                }
                let id = read_u64(payload, 0)?;
                let table_id = read_u64(payload, 8)?;
                if id == 0 || table_id == 0 || key[1..9] != id.to_be_bytes() {
                    return Err("column definition identity disagrees with its catalog key".into());
                }
                let name = catalog_name(payload, 20, 24, "column")?;
                if columns
                    .insert(id, CatalogColumn { table_id, name })
                    .is_some()
                {
                    return Err("catalog has duplicate column definitions".into());
                }
            }
            _ => {}
        }
    }
    let mut result = BTreeMap::new();
    for (name, definition) in definitions {
        let mappings = mappings
            .remove(&definition.id)
            .ok_or("index definition has no index-column mappings")?;
        if mappings.len() != usize::from(definition.column_count) {
            return Err("index-column mapping count disagrees with index definition".into());
        }
        let mut mapped_columns = Vec::with_capacity(mappings.len());
        for ordinal in 0..definition.column_count {
            let column_id = mappings
                .get(&ordinal)
                .copied()
                .ok_or("index-column mappings are not contiguous")?;
            let column = columns
                .get(&column_id)
                .ok_or("index-column mapping names an unknown column")?;
            if column.table_id != definition.table_id {
                return Err("index-column mapping names a column from another table".into());
            }
            mapped_columns.push(column.name.clone());
        }
        result.insert(
            name,
            ExplicitIndexDefinition {
                root: definition.root,
                unique: definition.unique,
                columns: mapped_columns,
            },
        );
    }
    if !mappings.is_empty() {
        return Err("index-column mapping has no explicit index definition".into());
    }
    Ok(result)
}

struct RawCatalogParts {
    next_object_id: u64,
    tables: BTreeMap<String, RawTable>,
    indexes: BTreeMap<String, RawIndex>,
    constraints: BTreeMap<String, RawConstraint>,
    table_constraints: BTreeMap<u64, Vec<u64>>,
}

#[derive(Clone)]
struct RawTableDefinition {
    id: u64,
    root: RawPageRef,
    next_rowid: u64,
    name: String,
}

#[derive(Clone)]
struct RawIndexDefinition {
    id: u64,
    table_id: u64,
    root: RawPageRef,
    unique: bool,
    owner_constraint_id: Option<u64>,
    name: String,
}

#[derive(Clone)]
struct RawConstraintDefinition {
    id: u64,
    table_id: u64,
    kind: u8,
    index_id: Option<u64>,
    check_program_len: u32,
    check_program_version: Option<u8>,
    name: String,
}

fn parse_raw_catalog(entries: &[(Vec<u8>, Vec<u8>)]) -> Result<RawCatalogParts, String> {
    let mut next_object_id = None;
    let mut table_definitions = BTreeMap::new();
    let mut column_definitions = BTreeMap::new();
    let mut table_columns = BTreeMap::<u64, BTreeMap<u16, u64>>::new();
    let mut index_definitions = BTreeMap::new();
    let mut index_columns = BTreeMap::<u64, BTreeMap<u16, u64>>::new();
    let mut constraint_definitions = BTreeMap::new();
    let mut constraint_names = BTreeMap::new();
    let mut constraint_columns = BTreeMap::<u64, BTreeMap<u16, u64>>::new();
    let mut table_constraints = BTreeMap::<u64, Vec<u64>>::new();

    for (key, value) in entries {
        let family = *key.first().ok_or("catalog contains an empty key")?;
        match family {
            0x00 => {
                if key.len() != 1 {
                    return Err("raw catalog meta key is malformed".into());
                }
                let payload = catalog_payload(value, family, "catalog meta")?;
                if payload.len() != 12 || read_u16(payload, 0)? != 1 || read_u16(payload, 2)? != 0 {
                    return Err("raw catalog meta payload is malformed".into());
                }
                if next_object_id.replace(read_u64(payload, 4)?).is_some() {
                    return Err("raw catalog has duplicate meta records".into());
                }
            }
            0x11 => {
                let id = raw_id_key(key, family, "table definition")?;
                let payload = catalog_payload(value, family, "table definition")?;
                if payload.len() < 44 || read_u64(payload, 0)? != id {
                    return Err("raw table definition identity is malformed".into());
                }
                let definition = RawTableDefinition {
                    id,
                    root: RawPageRef {
                        id: read_u64(payload, 16)?,
                        generation: read_u64(payload, 24)?,
                    },
                    next_rowid: read_u64(payload, 32)?,
                    name: catalog_name(payload, 42, 44, "table")?,
                };
                if table_definitions.insert(id, definition).is_some() {
                    return Err("raw catalog has duplicate table definitions".into());
                }
            }
            0x13 => {
                if key.len() != 11 {
                    return Err("raw column ordinal key is malformed".into());
                }
                let table_id = read_be_u64(key, 1)?;
                let ordinal = read_be_u16(key, 9)?;
                let column_id = raw_mapping_id(value, family, "column ordinal")?;
                if table_columns
                    .entry(table_id)
                    .or_default()
                    .insert(ordinal, column_id)
                    .is_some()
                {
                    return Err("raw catalog has duplicate column ordinals".into());
                }
            }
            0x14 => {
                let id = raw_id_key(key, family, "column definition")?;
                let payload = catalog_payload(value, family, "column definition")?;
                if payload.len() < 24 || read_u64(payload, 0)? != id || read_u16(payload, 22)? != 0
                {
                    return Err("raw column definition is malformed".into());
                }
                let column = RawColumn {
                    id,
                    ordinal: read_u16(payload, 16)?,
                    nullable: payload[19] & 1 != 0,
                    name: catalog_name(payload, 20, 24, "column")?,
                };
                if column_definitions
                    .insert(id, (read_u64(payload, 8)?, column))
                    .is_some()
                {
                    return Err("raw catalog has duplicate column definitions".into());
                }
            }
            0x21 => {
                let id = raw_id_key(key, family, "index definition")?;
                let payload = catalog_payload(value, family, "index definition")?;
                if payload.len() < 48 || read_u64(payload, 0)? != id || read_u16(payload, 36)? != 0
                {
                    return Err("raw index definition is malformed".into());
                }
                let owner_constraint_id = match (payload[35], read_u64(payload, 38)?) {
                    (0, 0) => None,
                    (1, id) if id != 0 => Some(id),
                    _ => return Err("raw index owner is malformed".into()),
                };
                let definition = RawIndexDefinition {
                    id,
                    table_id: read_u64(payload, 8)?,
                    root: RawPageRef {
                        id: read_u64(payload, 16)?,
                        generation: read_u64(payload, 24)?,
                    },
                    unique: match payload[34] {
                        0 => false,
                        1 => true,
                        _ => return Err("raw index uniqueness flag is malformed".into()),
                    },
                    owner_constraint_id,
                    name: catalog_name(payload, 46, 48, "index")?,
                };
                if index_definitions.insert(id, definition).is_some() {
                    return Err("raw catalog has duplicate index definitions".into());
                }
            }
            0x23 => {
                if key.len() != 11 {
                    return Err("raw index-column key is malformed".into());
                }
                let index_id = read_be_u64(key, 1)?;
                let ordinal = read_be_u16(key, 9)?;
                let column_id = raw_mapping_id(value, family, "index-column")?;
                if index_columns
                    .entry(index_id)
                    .or_default()
                    .insert(ordinal, column_id)
                    .is_some()
                {
                    return Err("raw catalog has duplicate index columns".into());
                }
            }
            0x30 => {
                let name = raw_global_name_key(key, family, "constraint name")?;
                let id = raw_mapping_id(value, family, "constraint name")?;
                if constraint_names.insert(name, id).is_some() {
                    return Err("raw catalog has duplicate constraint names".into());
                }
            }
            0x31 => {
                let id = raw_id_key(key, family, "constraint definition")?;
                let payload = catalog_payload(value, family, "constraint definition")?;
                if payload.len() < 56 || read_u64(payload, 0)? != id || payload[17] != 0 {
                    return Err("raw constraint definition is malformed".into());
                }
                let check_program_len = read_u32(payload, 48)?;
                let program_start = 56_usize;
                let program_end = program_start
                    .checked_add(
                        usize::try_from(check_program_len)
                            .map_err(|_| "raw check program length exceeds this platform")?,
                    )
                    .ok_or("raw check program length overflows")?;
                let check_program = payload
                    .get(program_start..program_end)
                    .ok_or("raw check program is truncated")?;
                let definition = RawConstraintDefinition {
                    id,
                    table_id: read_u64(payload, 8)?,
                    kind: payload[16],
                    index_id: match read_u64(payload, 20)? {
                        0 => None,
                        id => Some(id),
                    },
                    check_program_len,
                    check_program_version: check_program.first().copied(),
                    name: catalog_name(payload, 52, program_end, "constraint")?,
                };
                if constraint_definitions.insert(id, definition).is_some() {
                    return Err("raw catalog has duplicate constraint definitions".into());
                }
            }
            0x32 => {
                if key.len() != 17 {
                    return Err("raw table-constraint key is malformed".into());
                }
                let table_id = read_be_u64(key, 1)?;
                let constraint_id = read_be_u64(key, 9)?;
                if raw_mapping_id(value, family, "table-constraint")? != constraint_id {
                    return Err("raw table-constraint mapping disagrees with its key".into());
                }
                table_constraints
                    .entry(table_id)
                    .or_default()
                    .push(constraint_id);
            }
            0x33 => {
                if key.len() != 11 {
                    return Err("raw constraint-column key is malformed".into());
                }
                let constraint_id = read_be_u64(key, 1)?;
                let ordinal = read_be_u16(key, 9)?;
                let column_id = raw_mapping_id(value, family, "constraint-column")?;
                if constraint_columns
                    .entry(constraint_id)
                    .or_default()
                    .insert(ordinal, column_id)
                    .is_some()
                {
                    return Err("raw catalog has duplicate constraint columns".into());
                }
            }
            _ => {}
        }
    }

    let next_object_id = next_object_id.ok_or("raw catalog lacks its meta record")?;
    let mut tables = BTreeMap::new();
    for definition in table_definitions.into_values() {
        let columns = raw_ordered_columns(
            table_columns.remove(&definition.id).unwrap_or_default(),
            &column_definitions,
            definition.id,
            "table",
        )?;
        tables.insert(
            definition.name,
            RawTable {
                id: definition.id,
                root: definition.root,
                root_page_type: 0,
                next_rowid: definition.next_rowid,
                columns,
            },
        );
    }
    let mut indexes = BTreeMap::new();
    for definition in index_definitions.into_values() {
        let columns = raw_ordered_ids(
            index_columns.remove(&definition.id).unwrap_or_default(),
            "index",
        )?;
        indexes.insert(
            definition.name,
            RawIndex {
                id: definition.id,
                table_id: definition.table_id,
                root: definition.root,
                root_page_type: 0,
                unique: definition.unique,
                owner_constraint_id: definition.owner_constraint_id,
                columns,
            },
        );
    }
    let mut constraints = BTreeMap::new();
    for definition in constraint_definitions.into_values() {
        let name_mapping = constraint_names
            .get(&definition.name)
            .copied()
            .ok_or("raw constraint definition lacks a name mapping")?;
        if name_mapping != definition.id {
            return Err("raw constraint name mapping disagrees with definition".into());
        }
        constraints.insert(
            definition.name,
            RawConstraint {
                id: definition.id,
                table_id: definition.table_id,
                kind: definition.kind,
                index_id: definition.index_id,
                columns: raw_ordered_ids(
                    constraint_columns
                        .remove(&definition.id)
                        .unwrap_or_default(),
                    "constraint",
                )?,
                check_program_len: definition.check_program_len,
                check_program_version: definition.check_program_version,
            },
        );
    }
    for ids in table_constraints.values_mut() {
        ids.sort_unstable();
    }
    Ok(RawCatalogParts {
        next_object_id,
        tables,
        indexes,
        constraints,
        table_constraints,
    })
}

fn raw_ordered_columns(
    ids: BTreeMap<u16, u64>,
    definitions: &BTreeMap<u64, (u64, RawColumn)>,
    table_id: u64,
    label: &str,
) -> Result<Vec<RawColumn>, String> {
    let mut result = Vec::with_capacity(ids.len());
    for (expected, (ordinal, id)) in ids.into_iter().enumerate() {
        if usize::from(ordinal) != expected {
            return Err(format!("raw {label} columns are not contiguous"));
        }
        let (owner, column) = definitions
            .get(&id)
            .ok_or_else(|| format!("raw {label} column is undefined"))?;
        if *owner != table_id || column.ordinal != ordinal {
            return Err(format!(
                "raw {label} column owner or ordinal is inconsistent"
            ));
        }
        result.push(column.clone());
    }
    Ok(result)
}

fn raw_ordered_ids(ids: BTreeMap<u16, u64>, label: &str) -> Result<Vec<u64>, String> {
    let mut result = Vec::with_capacity(ids.len());
    for (expected, (ordinal, id)) in ids.into_iter().enumerate() {
        if usize::from(ordinal) != expected {
            return Err(format!("raw {label} columns are not contiguous"));
        }
        result.push(id);
    }
    Ok(result)
}

fn raw_id_key(key: &[u8], family: u8, label: &str) -> Result<u64, String> {
    if key.len() != 9 || key.first() != Some(&family) {
        return Err(format!("raw {label} key is malformed"));
    }
    read_be_u64(key, 1)
}

fn raw_global_name_key(key: &[u8], family: u8, label: &str) -> Result<String, String> {
    if key.first() != Some(&family) || key.len() < 3 {
        return Err(format!("raw {label} key is malformed"));
    }
    let length = usize::from(read_be_u16(key, 1)?);
    let end = 3_usize
        .checked_add(length)
        .ok_or_else(|| format!("raw {label} name length overflows"))?;
    if end != key.len() {
        return Err(format!("raw {label} key length is malformed"));
    }
    std::str::from_utf8(&key[3..end])
        .map(str::to_owned)
        .map_err(|_| format!("raw {label} name is not UTF-8"))
}

fn raw_mapping_id(value: &[u8], family: u8, label: &str) -> Result<u64, String> {
    let payload = catalog_payload(value, family, label)?;
    if payload.len() != 8 {
        return Err(format!("raw {label} payload is malformed"));
    }
    read_u64(payload, 0)
}

fn read_be_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset.checked_add(2).ok_or("integer offset overflow")?;
    Ok(u16::from_be_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated disk value")?
            .try_into()
            .map_err(|_| "truncated disk value")?,
    ))
}

fn read_be_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let end = offset.checked_add(8).ok_or("integer offset overflow")?;
    Ok(u64::from_be_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated disk value")?
            .try_into()
            .map_err(|_| "truncated disk value")?,
    ))
}

fn raw_freelist(disk: &Disk<'_>) -> Result<Vec<RawFreelistTrunk>, String> {
    let mut trunks = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = (disk.header.freelist_root != 0).then_some((disk.header.freelist_root, None));
    while let Some((page_id, expected_generation)) = current {
        if page_id >= disk.header.page_count || !seen.insert(page_id) {
            return Err("raw freelist root is outside the accepted page range or cyclic".into());
        }
        let start = ordinary_page_offset(page_id, disk.header.page_size)?;
        let end = start
            .checked_add(disk.header.page_size)
            .ok_or("raw freelist page extent overflows")?;
        let bytes = disk
            .bytes
            .get(start..end)
            .ok_or("raw freelist page lies outside accepted file")?;
        verify_page_checksum(bytes)?;
        if byte(bytes, 0)? != 6 || read_u64(bytes, 4)? != page_id {
            return Err("raw freelist page identity or kind is invalid".into());
        }
        let generation = read_u64(bytes, 12)?;
        if generation == 0 || expected_generation.is_some_and(|expected| expected != generation) {
            return Err("raw freelist generation is invalid".into());
        }
        let next_id = read_u64(bytes, 48)?;
        let next_generation = read_u64(bytes, 56)?;
        let leaf_count = usize::try_from(read_u32(bytes, 64)?)
            .map_err(|_| "raw freelist leaf count exceeds this platform")?;
        if read_u32(bytes, 68)? != 0 || leaf_count > (bytes.len() - 72) / 8 {
            return Err("raw freelist body is malformed".into());
        }
        let mut leaves = Vec::with_capacity(leaf_count);
        for position in 0..leaf_count {
            let offset = 72_usize
                .checked_add(
                    position
                        .checked_mul(8)
                        .ok_or("raw freelist leaf offset overflows")?,
                )
                .ok_or("raw freelist leaf offset overflows")?;
            leaves.push(read_u64(bytes, offset)?);
        }
        current = match (next_id, next_generation) {
            (0, 0) => None,
            (id, generation) if id != 0 && generation != 0 => Some((id, Some(generation))),
            _ => return Err("raw freelist next reference is malformed".into()),
        };
        trunks.push(RawFreelistTrunk {
            reference: RawPageRef {
                id: page_id,
                generation,
            },
            leaves,
        });
    }
    Ok(trunks)
}

fn catalog_payload<'a>(value: &'a [u8], kind: u8, label: &str) -> Result<&'a [u8], String> {
    if value.get(0..2) != Some(&[kind, 1]) || value.get(2..4) != Some(&[0, 0]) {
        return Err(format!("{label} envelope is malformed"));
    }
    let payload_len = usize::try_from(read_u32(value, 4)?)
        .map_err(|_| format!("{label} payload length exceeds platform"))?;
    let payload_end = 8usize
        .checked_add(payload_len)
        .ok_or_else(|| format!("{label} payload overflow"))?;
    if value.len() != payload_end {
        return Err(format!("{label} length disagrees with its envelope"));
    }
    Ok(&value[8..])
}

fn catalog_name(
    payload: &[u8],
    length_offset: usize,
    name_start: usize,
    label: &str,
) -> Result<String, String> {
    let name_len = usize::from(read_u16(payload, length_offset)?);
    let name_end = name_start
        .checked_add(name_len)
        .ok_or_else(|| format!("{label} name overflow"))?;
    let name = payload
        .get(name_start..name_end)
        .ok_or_else(|| format!("{label} name is truncated"))?;
    if name_end != payload.len() || name.is_empty() {
        return Err(format!("{label} name length is invalid"));
    }
    std::str::from_utf8(name)
        .map_err(|_| format!("{label} name is not UTF-8"))
        .map(str::to_owned)
}

fn assert_expected_index_metadata(
    name: &str,
    definition: &ExplicitIndexDefinition,
) -> Result<(), String> {
    let (unique, columns): (bool, &[&str]) = match name {
        "UX_T_ID" => (true, &["ID"]),
        "IX_T_G_ID" => (false, &["G", "ID"]),
        _ => return Ok(()),
    };
    if definition.unique != unique
        || definition
            .columns
            .iter()
            .map(String::as_str)
            .ne(columns.iter().copied())
    {
        return Err(format!(
            "{name} catalog uniqueness or column mapping is incorrect"
        ));
    }
    Ok(())
}

fn assert_selected_physical_pairs(
    name: &str,
    entries: &[(Vec<u8>, Vec<u8>)],
) -> Result<(), String> {
    let wanted = match name {
        "UX_T_ID" => [1000_i64, 99_000, 99_999]
            .into_iter()
            .map(|id| physical_int_key(&[id], rowid_for_test_id(id)))
            .collect::<Vec<_>>(),
        "IX_T_G_ID" => [1000_i64, 99_000, 99_999]
            .into_iter()
            .map(|id| {
                let group = id % 101 + i64::from(id >= 99_000);
                physical_int_key(&[group, id], rowid_for_test_id(id))
            })
            .collect::<Vec<_>>(),
        _ => return Ok(()),
    };
    for key in wanted {
        if entries
            .binary_search_by(|(actual, _)| actual.cmp(&key))
            .is_err()
        {
            return Err(format!(
                "{name} misses a selected logical-key/ROWID physical pair"
            ));
        }
    }
    Ok(())
}

fn physical_int_key(values: &[i64], rowid: u64) -> Vec<u8> {
    let component_bytes = values
        .len()
        .checked_mul(9)
        .expect("physical key component bytes fit");
    let capacity = component_bytes
        .checked_add(8)
        .expect("physical key capacity fits");
    let mut key = Vec::with_capacity(capacity);
    for value in values {
        key.push(0);
        let bits = u64::from_be_bytes(value.to_be_bytes());
        key.extend_from_slice(&(bits ^ (1_u64 << 63)).to_be_bytes());
    }
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

fn rowid_for_test_id(id: i64) -> u64 {
    u64::try_from(id)
        .expect("selected scale-row id is nonnegative")
        .checked_add(1)
        .expect("selected scale-row ROWID fits")
}

fn decode_leaf_sibling(page: &Page<'_>, page_count: u64) -> Result<Option<PageRef>, String> {
    if page.metadata.len() != 8 {
        return Err("leaf metadata length is invalid".into());
    }
    let generation = read_u64(page.metadata, 0)?;
    match (page.right_sibling, generation) {
        (None, 0) => Ok(None),
        (Some(id), generation) if generation != 0 && id < page_count => {
            Ok(Some(PageRef { id, generation }))
        }
        _ => Err("leaf sibling page id and generation disagree".into()),
    }
}

fn decode_leaf_cell(cell: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    if cell.len() < 12 {
        return Err("leaf cell is truncated".into());
    }
    let key_len = usize::from(read_u16(cell, 0)?);
    if key_len > 1024 || cell[3] != 0 {
        return Err("leaf cell key length or reserved byte is invalid".into());
    }
    let value_len =
        usize::try_from(read_u64(cell, 4)?).map_err(|_| "leaf value length exceeds platform")?;
    if cell[2] != 0 || value_len > 16 * 1024 * 1024 {
        return Err("leaf cell uses unsupported storage or an excessive value".into());
    }
    let key_end = 12usize
        .checked_add(key_len)
        .ok_or("leaf key extent overflow")?;
    let value_end = key_end
        .checked_add(value_len)
        .ok_or("leaf value extent overflow")?;
    if value_end != cell.len() {
        return Err("leaf cell length does not match key/value lengths".into());
    }
    Ok((
        cell[12..key_end].to_vec(),
        cell[key_end..value_end].to_vec(),
    ))
}

fn decode_internal_children(
    page: &Page<'_>,
    page_count: u64,
) -> Result<(Vec<PageRef>, Vec<Vec<u8>>), String> {
    if page.metadata.len() != 16 {
        return Err("internal metadata length is invalid".into());
    }
    let mut children = vec![PageRef {
        id: read_u64(page.metadata, 0)?,
        generation: read_u64(page.metadata, 8)?,
    }];
    let mut keys = Vec::with_capacity(page.cells.len());
    for cell in &page.cells {
        if cell.len() < 20 || read_u16(cell, 2)? != 0 {
            return Err("internal cell is truncated or has reserved bytes".into());
        }
        let key_len = usize::from(read_u16(cell, 0)?);
        if key_len > 1024
            || cell.len()
                != 20usize
                    .checked_add(key_len)
                    .ok_or("internal key extent overflow")?
        {
            return Err("internal cell key length is invalid".into());
        }
        children.push(PageRef {
            id: read_u64(cell, 4)?,
            generation: read_u64(cell, 12)?,
        });
        keys.push(cell[20..].to_vec());
    }
    if children.iter().any(|reference| {
        reference.id == 0 || reference.id >= page_count || reference.generation == 0
    }) {
        return Err("internal child reference is outside accepted page range".into());
    }
    Ok((children, keys))
}

fn select_header(bytes: &[u8]) -> Result<Header, String> {
    let a = decode_header(bytes.get(..SUPERBLOCK_LEN).ok_or("missing superblock A")?);
    let b = decode_header(
        bytes
            .get(SUPERBLOCK_LEN..SUPERBLOCK_AREA_LEN)
            .ok_or("missing superblock B")?,
    );
    match (a, b) {
        (Ok(a), Ok(b))
            if a.page_size == b.page_size && a.publish_sequence != b.publish_sequence =>
        {
            Ok(if a.publish_sequence > b.publish_sequence {
                a
            } else {
                b
            })
        }
        (Ok(a), Ok(b)) if a == b => Ok(a),
        (Ok(_), Ok(_)) => Err("superblock slots disagree".into()),
        (Ok(a), Err(_)) | (Err(_), Ok(a)) => Ok(a),
        (Err(a), Err(b)) => Err(format!("both superblock slots are invalid: A={a}; B={b}")),
    }
}

impl PartialEq for Header {
    fn eq(&self, other: &Self) -> bool {
        self.page_size == other.page_size
            && self.publish_sequence == other.publish_sequence
            && self.checkpoint_lsn == other.checkpoint_lsn
            && self.page_count == other.page_count
            && self.catalog_root == other.catalog_root
            && self.freelist_root == other.freelist_root
            && self.features == other.features
    }
}

fn decode_header(bytes: &[u8]) -> Result<Header, String> {
    if bytes.len() != SUPERBLOCK_LEN
        || &bytes[..16] != b"ORA-SQLLITE-SB\0\0"
        || crc32c(&bytes[..SUPERBLOCK_LEN - 4]) != read_u32(bytes, SUPERBLOCK_LEN - 4)?
    {
        return Err("superblock magic or checksum is invalid".into());
    }
    if read_u16(bytes, 16)? != 2
        || read_u16(bytes, 18)? != 0
        || bytes[96..SUPERBLOCK_LEN - 4].iter().any(|byte| *byte != 0)
    {
        return Err("unsupported format-2 superblock fields".into());
    }
    let page_size = usize::try_from(read_u32(bytes, 20)?)
        .map_err(|_| "superblock page size exceeds platform")?;
    if !matches!(page_size, 4096 | 8192 | 16384)
        || read_u64(bytes, 40)? == 0
        || read_u64(bytes, 80)? == 0
    {
        return Err("superblock geometry is invalid".into());
    }
    let page_count = read_u64(bytes, 56)?;
    let catalog_root = read_u64(bytes, 64)?;
    let features = read_u64(bytes, 88)?;
    if page_count == 0
        || catalog_root == 0
        || catalog_root >= page_count
        || features & !(FEATURE_CATALOG_V1 | FEATURE_INDEX_V1) != 0
    {
        return Err("superblock root or features are invalid".into());
    }
    Ok(Header {
        page_size,
        publish_sequence: read_u64(bytes, 40)?,
        checkpoint_lsn: read_u64(bytes, 48)?,
        page_count,
        catalog_root,
        freelist_root: read_u64(bytes, 72)?,
        features,
    })
}

fn ordinary_page_offset(id: u64, page_size: usize) -> Result<usize, String> {
    let ordinary = id
        .checked_sub(1)
        .ok_or("page zero is not an ordinary page")?;
    SUPERBLOCK_AREA_LEN
        .checked_add(
            usize::try_from(ordinary)
                .map_err(|_| "page id exceeds platform")?
                .checked_mul(page_size)
                .ok_or("page offset overflow")?,
        )
        .ok_or_else(|| "page offset overflow".into())
}

fn verify_page_checksum(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 40 || read_u32(bytes, 36)? != crc32c_parts(&bytes[..36], &bytes[40..]) {
        return Err("page checksum is invalid".into());
    }
    Ok(())
}

fn crc32c_parts(first: &[u8], second: &[u8]) -> u32 {
    let mut state = !0_u32;
    crc32c_update(&mut state, first);
    crc32c_update(&mut state, &[0; 4]);
    crc32c_update(&mut state, second);
    !state
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut state = !0_u32;
    crc32c_update(&mut state, bytes);
    !state
}

fn crc32c_update(state: &mut u32, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u32::from(*byte);
        for _ in 0..8 {
            *state = if *state & 1 == 1 {
                (*state >> 1) ^ 0x82f6_3b78
            } else {
                *state >> 1
            };
        }
    }
}

fn byte(bytes: &[u8], offset: usize) -> Result<u8, String> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| "truncated disk value".into())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset.checked_add(2).ok_or("integer offset overflow")?;
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated disk value")?
            .try_into()
            .map_err(|_| "truncated disk value")?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset.checked_add(4).ok_or("integer offset overflow")?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated disk value")?
            .try_into()
            .map_err(|_| "truncated disk value")?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let end = offset.checked_add(8).ok_or("integer offset overflow")?;
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or("truncated disk value")?
            .try_into()
            .map_err(|_| "truncated disk value")?,
    ))
}
