use std::collections::BTreeSet;

use super::{
    PageId, PageRef,
    btree::{
        BTree, INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, LeafCell, TreeKind,
        ValueStorage, decode_internal_leftmost, decode_leaf_right_sibling,
    },
    freelist::Freelist,
    overflow,
    pager::PageSource,
    slotted::SlottedPage,
};
use crate::{DbError, ErrorCode, Result};

#[derive(Debug, Default)]
pub(super) struct ValidationReport {
    pub(super) tree_pages: BTreeSet<PageId>,
    pub(super) overflow_pages: BTreeSet<PageId>,
}

impl ValidationReport {
    pub(super) fn all_live_pages(&self) -> BTreeSet<PageId> {
        self.tree_pages
            .union(&self.overflow_pages)
            .copied()
            .collect()
    }
}

/// Normal catalog open checks user root identities only, never their descendants.
pub(super) fn validate_table_roots(
    source: &mut impl PageSource,
    roots: impl IntoIterator<Item = PageRef>,
    live: &mut BTreeSet<PageId>,
) -> Result<()> {
    let page_count = source.page_count().map_err(as_corruption)?;
    for root in roots {
        validate_page_id(root.page_id(), page_count, "table root")?;
        if !live.insert(root.page_id()) {
            return Err(corruption(
                "table root overlaps another live catalog page or table root",
            ));
        }
        let handle = source.read_page(root).map_err(as_corruption)?;
        let header = handle.header();
        if header.page_id() != root.page_id()
            || header.generation() != root.generation()
            || TreeKind::from_root_page_type(header.page_type()) != Some(TreeKind::Table)
        {
            return Err(corruption(
                "table root has an invalid type or page reference",
            ));
        }
    }
    Ok(())
}

/// Extend a catalog's live set only after proving each user tree is disjoint.
/// Freelist validation is deliberately left to the caller, once for the full set.
pub(super) fn validate_table_trees(
    source: &mut impl PageSource,
    roots: impl IntoIterator<Item = PageRef>,
    live: &mut BTreeSet<PageId>,
) -> Result<()> {
    for root in roots {
        let pages =
            validate_tree(source, &BTree::from_root(TreeKind::Table, root))?.all_live_pages();
        if !live.is_disjoint(&pages) {
            return Err(corruption(
                "user tree overlaps another live catalog or user tree",
            ));
        }
        live.extend(pages);
    }
    Ok(())
}

#[derive(Default)]
struct Subtree {
    minimum: Option<Vec<u8>>,
    maximum: Option<Vec<u8>>,
    leaf_depth: Option<u64>,
    leaves: Vec<(PageRef, Option<PageRef>)>,
    tree_pages: BTreeSet<PageId>,
    overflow_pages: BTreeSet<PageId>,
}

struct TreeWalk {
    kind: TreeKind,
    page_count: u64,
    seen_tree_pages: BTreeSet<PageId>,
    seen_overflow_pages: BTreeSet<PageId>,
}

struct PendingInternal {
    cells: Vec<InternalCell>,
    children: Vec<PageRef>,
    next_child: usize,
    result: Subtree,
    previous_maximum: Option<Vec<u8>>,
}

enum ValidationNode {
    Leaf {
        cells: Vec<LeafCell>,
        right_sibling: Option<PageRef>,
    },
    Internal {
        leftmost: PageRef,
        cells: Vec<InternalCell>,
    },
}

pub(super) fn validate_tree(
    source: &mut impl PageSource,
    tree: &BTree,
) -> Result<ValidationReport> {
    let page_count = source.page_count().map_err(as_corruption)?;
    if page_count < 2 {
        return Err(corruption("database has no ordinary pages"));
    }

    let root = tree.root();
    let root_page_type = {
        let root_handle = source.read_page(root).map_err(as_corruption)?;
        root_handle.header().page_type()
    };
    if TreeKind::from_root_page_type(root_page_type) != Some(tree.kind()) {
        return Err(corruption(
            "B+Tree root page type does not match its tree kind",
        ));
    }
    let mut walk = TreeWalk {
        kind: tree.kind(),
        page_count,
        seen_tree_pages: BTreeSet::new(),
        seen_overflow_pages: BTreeSet::new(),
    };
    let subtree = walk.walk_tree(source, root, 0, true)?;
    validate_leaf_chain(&subtree.leaves)?;
    if !subtree.tree_pages.is_disjoint(&subtree.overflow_pages) {
        return Err(corruption("B+Tree and overflow page sets overlap"));
    }
    Ok(ValidationReport {
        tree_pages: subtree.tree_pages,
        overflow_pages: subtree.overflow_pages,
    })
}

impl TreeWalk {
    fn walk_tree(
        &mut self,
        source: &mut impl PageSource,
        reference: PageRef,
        depth: u64,
        is_root: bool,
    ) -> Result<Subtree> {
        let mut stack = Vec::new();
        let mut next = Some((reference, depth, is_root));
        let mut completed = None;

        loop {
            if let Some((reference, depth, is_root)) = next.take() {
                match self.open_subtree(source, reference, depth, is_root)? {
                    Ok(leaf) => completed = Some(leaf),
                    Err(internal) => {
                        stack
                            .try_reserve(1)
                            .map_err(|_| corruption("B+Tree validation stack allocation failed"))?;
                        let child = internal.children[0];
                        stack.push(internal);
                        next = Some((
                            child,
                            depth
                                .checked_add(1)
                                .ok_or_else(|| corruption("B+Tree depth overflow"))?,
                            false,
                        ));
                    }
                }
                continue;
            }

            let child = completed
                .take()
                .ok_or_else(|| corruption("B+Tree validation lost a subtree result"))?;
            let Some(parent) = stack.last_mut() else {
                return Ok(child);
            };
            parent.accept_child(child)?;
            if parent.next_child < parent.children.len() {
                next = Some((
                    parent.children[parent.next_child],
                    depth_for_stack(&stack)?,
                    false,
                ));
            } else {
                completed = stack.pop().map(|parent| parent.result);
            }
        }
    }

    fn open_subtree(
        &mut self,
        source: &mut impl PageSource,
        reference: PageRef,
        depth: u64,
        is_root: bool,
    ) -> Result<std::result::Result<Subtree, PendingInternal>> {
        validate_page_id(reference.page_id(), self.page_count, "B+Tree")?;
        if !self.seen_tree_pages.insert(reference.page_id()) {
            return Err(corruption("B+Tree has duplicate reachable page references"));
        }
        match load_validation_node(source, self.kind, reference)? {
            ValidationNode::Leaf {
                cells,
                right_sibling,
            } => {
                if !is_root && cells.is_empty() {
                    return Err(corruption("non-root B+Tree leaf is empty"));
                }
                let mut result = Subtree {
                    leaf_depth: Some(depth),
                    ..Subtree::default()
                };
                result.tree_pages.insert(reference.page_id());
                result.leaves.push((reference, right_sibling));
                for cell in &cells {
                    if let ValueStorage::Overflow(first) = cell.storage() {
                        let chain = overflow::collect_chain(source, *first, cell.total_value_len())
                            .map_err(as_corruption)?;
                        for overflow_ref in chain {
                            validate_page_id(overflow_ref.page_id(), self.page_count, "overflow")?;
                            if !self.seen_overflow_pages.insert(overflow_ref.page_id()) {
                                return Err(corruption(
                                    "overflow page is referenced more than once",
                                ));
                            }
                            result.overflow_pages.insert(overflow_ref.page_id());
                        }
                    }
                }
                if let Some(first) = cells.first() {
                    result.minimum = Some(first.key().to_vec());
                    result.maximum = cells.last().map(|cell| cell.key().to_vec());
                }
                Ok(Ok(result))
            }
            ValidationNode::Internal { leftmost, cells } => {
                if cells.is_empty() {
                    return Err(corruption("B+Tree internal page has no separator cells"));
                }
                let mut children = Vec::new();
                children
                    .try_reserve(cells.len() + 1)
                    .map_err(|_| corruption("B+Tree child list allocation failed"))?;
                children.push(leftmost);
                children.extend(cells.iter().map(|cell| cell.right()));
                let mut result = Subtree::default();
                result.tree_pages.insert(reference.page_id());
                Ok(Err(PendingInternal {
                    cells,
                    children,
                    next_child: 0,
                    result,
                    previous_maximum: None,
                }))
            }
        }
    }
}

impl PendingInternal {
    fn accept_child(&mut self, child: Subtree) -> Result<()> {
        let child_minimum = child
            .minimum
            .as_ref()
            .ok_or_else(|| corruption("non-root B+Tree subtree is empty"))?;
        let child_maximum = child
            .maximum
            .as_ref()
            .ok_or_else(|| corruption("non-root B+Tree subtree is empty"))?;
        if let Some(previous) = &self.previous_maximum {
            if previous >= child_minimum {
                return Err(corruption(
                    "B+Tree child key ranges overlap or are unsorted",
                ));
            }
        }
        if self.next_child > 0 && self.cells[self.next_child - 1].key() != child_minimum {
            return Err(corruption(
                "B+Tree parent separator is not its right child minimum",
            ));
        }
        let child_depth = child
            .leaf_depth
            .ok_or_else(|| corruption("B+Tree subtree has no leaf depth"))?;
        if let Some(expected_depth) = self.result.leaf_depth {
            if expected_depth != child_depth {
                return Err(corruption("B+Tree leaves are at unequal depths"));
            }
        }
        self.result.leaf_depth = Some(child_depth);
        if self.result.minimum.is_none() {
            self.result.minimum = Some(child_minimum.clone());
        }
        self.result.maximum = Some(child_maximum.clone());
        self.previous_maximum = Some(child_maximum.clone());
        self.result.leaves.extend(child.leaves);
        self.result.tree_pages.extend(child.tree_pages);
        self.result.overflow_pages.extend(child.overflow_pages);
        self.next_child = self
            .next_child
            .checked_add(1)
            .ok_or_else(|| corruption("B+Tree child index overflow"))?;
        Ok(())
    }
}

fn depth_for_stack(stack: &[PendingInternal]) -> Result<u64> {
    u64::try_from(stack.len()).map_err(|_| corruption("B+Tree depth exceeds u64"))
}

fn load_validation_node(
    source: &mut impl PageSource,
    kind: TreeKind,
    reference: PageRef,
) -> Result<ValidationNode> {
    let handle = source.read_page(reference).map_err(as_corruption)?;
    let mut frame = super::page::PageFrame::decode(
        handle.bytes().to_vec(),
        reference.page_id(),
        None,
        Some(reference.generation()),
    )?;
    if frame.header().page_id() != reference.page_id()
        || frame.header().generation() != reference.generation()
    {
        return Err(corruption(
            "B+Tree page reference does not match the loaded page",
        ));
    }
    if frame.header().page_type() == kind.leaf_page_type() {
        let right_sibling = decode_leaf_right_sibling(
            &frame.bytes()[LEAF_METADATA_END - 8..LEAF_METADATA_END],
            frame.header().right_sibling(),
        )
        .map_err(as_corruption)?;
        let cells = SlottedPage::open(&mut frame, LEAF_METADATA_END)
            .map_err(as_corruption)?
            .cells()
            .map_err(as_corruption)?
            .into_iter()
            .map(|cell| LeafCell::decode(&cell).map_err(as_corruption))
            .collect::<Result<Vec<_>>>()?;
        require_strict_keys(cells.iter().map(LeafCell::key))?;
        return Ok(ValidationNode::Leaf {
            cells,
            right_sibling,
        });
    }
    if frame.header().page_type() == kind.internal_page_type() {
        let leftmost = decode_internal_leftmost(
            &frame.bytes()[INTERNAL_METADATA_END - 16..INTERNAL_METADATA_END],
        )
        .map_err(as_corruption)?;
        let cells = SlottedPage::open(&mut frame, INTERNAL_METADATA_END)
            .map_err(as_corruption)?
            .cells()
            .map_err(as_corruption)?
            .into_iter()
            .map(|cell| InternalCell::decode(&cell).map_err(as_corruption))
            .collect::<Result<Vec<_>>>()?;
        require_strict_keys(cells.iter().map(InternalCell::key))?;
        return Ok(ValidationNode::Internal { leftmost, cells });
    }
    Err(corruption("B+Tree node has an unexpected page type"))
}

fn require_strict_keys<'a>(mut keys: impl Iterator<Item = &'a [u8]>) -> Result<()> {
    let Some(mut previous) = keys.next() else {
        return Ok(());
    };
    for key in keys {
        if previous >= key {
            return Err(corruption("B+Tree page keys are not strictly ordered"));
        }
        previous = key;
    }
    Ok(())
}

fn validate_leaf_chain(leaves: &[(PageRef, Option<PageRef>)]) -> Result<()> {
    for (index, (reference, right_sibling)) in leaves.iter().enumerate() {
        let expected = leaves.get(index + 1).map(|(next, _)| *next);
        if *right_sibling != expected {
            return Err(corruption(
                "B+Tree leaf sibling chain is incomplete, cyclic, or unordered",
            ));
        }
        if reference.page_id().0 == 0 {
            return Err(corruption("B+Tree leaf chain contains the database header"));
        }
    }
    Ok(())
}

pub(super) fn validate_freelist(
    source: &mut impl PageSource,
    excluded: &BTreeSet<PageId>,
) -> Result<BTreeSet<PageId>> {
    let page_count = source.page_count().map_err(as_corruption)?;
    let mut all_free = BTreeSet::new();
    let mut trunks = BTreeSet::new();
    let mut leaves = BTreeSet::new();
    let mut current = source.freelist_root().map_err(as_corruption)?;

    while current.0 != 0 {
        validate_page_id(current, page_count, "freelist")?;
        if !trunks.insert(current) {
            return Err(corruption("freelist trunk chain has a cycle"));
        }
        let handle = source.read_page_by_id(current).map_err(as_corruption)?;
        let frame = handle.as_ref();
        PageRef::new(current, frame.header().generation()).map_err(as_corruption)?;
        if frame.header().page_id() != current {
            return Err(corruption(
                "freelist trunk page id does not match its frame",
            ));
        }
        let trunk = Freelist::decode(frame).map_err(as_corruption)?;
        for page_id in trunk.leaves() {
            validate_page_id(*page_id, page_count, "freelist")?;
            if !leaves.insert(*page_id) {
                return Err(corruption("freelist has duplicate leaf page ids"));
            }
            let leaf = source.read_page_by_id(*page_id).map_err(as_corruption)?;
            let leaf = leaf.as_ref();
            if leaf.header().page_id() != *page_id
                || leaf.header().generation() == 0
                || leaf.header().page_type() == super::page::PageType::Freelist
            {
                return Err(corruption(
                    "freelist leaf reference does not match a freeable page",
                ));
            }
        }
        let next_reference = trunk.next();
        if let Some(next_reference) = next_reference {
            let next_handle = source
                .read_page_by_id(next_reference.page_id())
                .map_err(as_corruption)?;
            let next_frame = next_handle.as_ref();
            if next_frame.header().generation() != next_reference.generation() {
                return Err(corruption(
                    "freelist trunk generation does not match its reference",
                ));
            }
        }
        current = next_reference.map_or(PageId(0), PageRef::page_id);
    }

    if !trunks.is_disjoint(&leaves) {
        return Err(corruption("freelist trunk and leaf page sets overlap"));
    }
    all_free.extend(&trunks);
    all_free.extend(&leaves);
    if !all_free.is_disjoint(excluded) {
        return Err(corruption(
            "freelist overlaps a live B+Tree or overflow page",
        ));
    }
    Ok(all_free)
}

fn validate_page_id(page_id: PageId, page_count: u64, label: &str) -> Result<()> {
    if page_id.0 == 0 || page_id.0 >= page_count {
        return Err(corruption(format!(
            "{label} page reference is outside the database"
        )));
    }
    Ok(())
}

fn as_corruption(error: DbError) -> DbError {
    match error.code() {
        ErrorCode::Busy | ErrorCode::Io | ErrorCode::Corruption => error,
        _ => corruption("storage structure cannot be proven valid"),
    }
}

fn corruption(message: impl Into<String>) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Arc,
    };

    use super::super::{
        PageId, PageRef,
        btree::{
            BTree, InternalCell, LeafCell, TreeKind, ValueStorage, encode_internal_leftmost,
            encode_leaf_right_sibling,
        },
        cache::PageHandle,
        freelist::Freelist,
        page::{PageFrame, PageType},
        pager::PageSource,
        slotted::SlottedPage,
    };
    use super::{validate_freelist, validate_tree};
    use crate::{DbError, ErrorCode, Result};

    #[test]
    fn valid_tree_reports_disjoint_page_sets() {
        let mut source = valid_source();
        let tree = BTree::from_root(TreeKind::Table, reference(1));

        let report = validate_tree(&mut source, &tree).expect("validate tree");
        assert!(report.tree_pages.is_disjoint(&report.overflow_pages));
        let free = validate_freelist(&mut source, &report.all_live_pages()).expect("validate free");
        assert!(free.is_disjoint(&report.all_live_pages()));
    }

    #[test]
    fn whole_tree_validator_rejects_every_structural_corruption_fixture() {
        let tree = BTree::from_root(TreeKind::Table, reference(1));
        type CorruptionCase = (&'static str, Box<dyn FnOnce(&mut TestSource)>);
        let cases: Vec<CorruptionCase> = vec![
            (
                "duplicate reachable child",
                Box::new(|source| {
                    source.put(internal_frame(
                        reference(1),
                        reference(2),
                        &[(b"c", reference(2))],
                    ));
                }),
            ),
            (
                "unequal leaf depth",
                Box::new(|source| {
                    source.put(internal_frame(
                        reference(1),
                        reference(4),
                        &[(b"c", reference(3))],
                    ));
                }),
            ),
            (
                "bad separator minimum",
                Box::new(|source| {
                    source.put(internal_frame(
                        reference(1),
                        reference(2),
                        &[(b"z", reference(3))],
                    ));
                }),
            ),
            (
                "unsorted page",
                Box::new(|source| {
                    source.put(leaf_frame(
                        reference(4),
                        &[(b"b", inline(b"b")), (b"a", inline(b"a"))],
                        Some(reference(5)),
                    ));
                }),
            ),
            (
                "sibling cycle",
                Box::new(|source| {
                    source.put(leaf_frame(
                        reference(7),
                        &[(b"d", inline(b"d"))],
                        Some(reference(4)),
                    ));
                }),
            ),
            (
                "missing sibling leaf",
                Box::new(|source| {
                    source.put(leaf_frame(
                        reference(4),
                        &[(b"a", overflow(reference(8)))],
                        Some(reference(6)),
                    ));
                }),
            ),
            (
                "out of range reference",
                Box::new(|source| {
                    source.put(internal_frame(
                        reference(1),
                        reference(2),
                        &[(
                            b"c",
                            PageRef::new(PageId(99), 1).expect("out of range reference"),
                        )],
                    ));
                }),
            ),
        ];

        for (name, corrupt) in cases {
            let mut source = valid_source();
            corrupt(&mut source);
            assert_eq!(
                validate_tree(&mut source, &tree).expect_err(name).code(),
                ErrorCode::Corruption,
                "{name} must be corruption",
            );
        }
    }

    #[test]
    fn freelist_validator_rejects_live_overflow_overlap_and_duplicate_leaf() {
        let tree = BTree::from_root(TreeKind::Table, reference(1));
        let mut overlap = valid_source();
        overlap.put(freelist_frame(reference(9), None, &[PageId(8)]));
        let live = validate_tree(&mut overlap, &tree)
            .expect("tree remains valid")
            .all_live_pages();
        assert_eq!(
            validate_freelist(&mut overlap, &live)
                .expect_err("overflow must not be free")
                .code(),
            ErrorCode::Corruption
        );

        let mut duplicate = valid_source();
        duplicate.page_count = 12;
        duplicate.put(freelist_frame(
            reference(9),
            Some(reference(11)),
            &[PageId(10)],
        ));
        duplicate.put(freelist_frame(reference(11), None, &[PageId(10)]));
        assert_eq!(
            validate_freelist(&mut duplicate, &BTreeSet::new())
                .expect_err("duplicate freelist leaf")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn tree_validator_rejects_a_supplied_kind_that_differs_from_its_root_page_type() {
        let mut source = valid_source();
        let table_root_as_index = BTree::from_root(TreeKind::Index, reference(1));

        assert_eq!(
            validate_tree(&mut source, &table_root_as_index)
                .expect_err("table root cannot validate as an index tree")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn tree_validator_rejects_a_deep_spine_without_recursing_on_the_process_stack() {
        let mut source = deep_spine_source(512);
        let tree = BTree::from_root(TreeKind::Table, reference(1));

        assert_eq!(
            validate_tree(&mut source, &tree)
                .expect_err("deep malformed spine")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn tree_validator_preserves_an_io_error_from_its_page_source() {
        let mut source = IoSource;
        let tree = BTree::from_root(TreeKind::Table, reference(1));

        assert_eq!(
            validate_tree(&mut source, &tree)
                .expect_err("source I/O must remain distinguishable from corruption")
                .code(),
            ErrorCode::Io
        );
    }

    fn valid_source() -> TestSource {
        let mut source = TestSource::new(11, PageId(9));
        source.put(internal_frame(
            reference(1),
            reference(2),
            &[(b"c", reference(3))],
        ));
        source.put(internal_frame(
            reference(2),
            reference(4),
            &[(b"b", reference(5))],
        ));
        source.put(internal_frame(
            reference(3),
            reference(6),
            &[(b"d", reference(7))],
        ));
        source.put(leaf_frame(
            reference(4),
            &[(b"a", overflow(reference(8)))],
            Some(reference(5)),
        ));
        source.put(leaf_frame(
            reference(5),
            &[(b"b", inline(b"b"))],
            Some(reference(6)),
        ));
        source.put(leaf_frame(
            reference(6),
            &[(b"c", inline(b"c"))],
            Some(reference(7)),
        ));
        source.put(leaf_frame(reference(7), &[(b"d", inline(b"d"))], None));
        source.put(overflow_frame(reference(8)));
        source.put(freelist_frame(reference(9), None, &[PageId(10)]));
        source.put(leaf_frame(reference(10), &[(b"x", inline(b"x"))], None));
        source
    }

    fn deep_spine_source(depth: u64) -> TestSource {
        let first_leaf = depth + 1;
        let first_right_leaf = first_leaf + 1;
        let mut source = TestSource::new(first_right_leaf + depth, PageId(0));
        for offset in 0..depth {
            let page_id = offset + 1;
            let leftmost = if offset + 1 == depth {
                reference(first_leaf)
            } else {
                reference(page_id + 1)
            };
            source.put(internal_frame(
                reference(page_id),
                leftmost,
                &[(b"b", reference(first_right_leaf + offset))],
            ));
            source.put(leaf_frame(
                reference(first_right_leaf + offset),
                &[(b"b", inline(b"b"))],
                None,
            ));
        }
        source.put(leaf_frame(
            reference(first_leaf),
            &[(b"a", inline(b"a"))],
            None,
        ));
        source
    }

    fn reference(page_id: u64) -> PageRef {
        PageRef::new(PageId(page_id), 1).expect("test page reference")
    }

    fn inline(value: &[u8]) -> ValueStorage {
        ValueStorage::Inline(value.to_vec())
    }

    fn overflow(first: PageRef) -> ValueStorage {
        ValueStorage::Overflow(first)
    }

    fn leaf_frame(
        reference: PageRef,
        cells: &[(&[u8], ValueStorage)],
        right_sibling: Option<PageRef>,
    ) -> PageFrame {
        let mut frame =
            PageFrame::new_zeroed(PageType::TableLeaf, reference, 4096).expect("leaf frame");
        frame
            .set_right_sibling(right_sibling.map(PageRef::page_id))
            .expect("set sibling page id");
        frame.bytes_mut()[48..56].copy_from_slice(&encode_leaf_right_sibling(right_sibling));
        {
            let mut page = SlottedPage::open(&mut frame, 56).expect("open leaf page");
            for (index, (key, storage)) in cells.iter().enumerate() {
                let cell = match storage {
                    ValueStorage::Inline(value) => LeafCell::inline(key.to_vec(), value.clone()),
                    ValueStorage::Overflow(first) => LeafCell::overflow(key.to_vec(), 1, *first),
                }
                .expect("leaf cell");
                page.insert(index, &cell.encode().expect("encode leaf cell"))
                    .expect("insert leaf cell");
            }
        }
        frame.prepare_for_write().expect("stamp leaf frame");
        frame
    }

    fn internal_frame(
        reference: PageRef,
        leftmost: PageRef,
        cells: &[(&[u8], PageRef)],
    ) -> PageFrame {
        let mut frame = PageFrame::new_zeroed(PageType::TableInternal, reference, 4096)
            .expect("internal frame");
        frame.bytes_mut()[48..64].copy_from_slice(&encode_internal_leftmost(leftmost));
        {
            let mut page = SlottedPage::open(&mut frame, 64).expect("open internal page");
            for (index, (key, right)) in cells.iter().enumerate() {
                let cell = InternalCell::new(key.to_vec(), *right).expect("internal cell");
                page.insert(index, &cell.encode().expect("encode internal cell"))
                    .expect("insert internal cell");
            }
        }
        frame.prepare_for_write().expect("stamp internal frame");
        frame
    }

    fn overflow_frame(reference: PageRef) -> PageFrame {
        let mut frame =
            PageFrame::new_zeroed(PageType::Overflow, reference, 4096).expect("overflow frame");
        frame.bytes_mut()[64..68].copy_from_slice(&1u32.to_le_bytes());
        frame.bytes_mut()[72] = b'a';
        frame.prepare_for_write().expect("stamp overflow frame");
        frame
    }

    fn freelist_frame(reference: PageRef, next: Option<PageRef>, leaves: &[PageId]) -> PageFrame {
        let mut frame =
            PageFrame::new_zeroed(PageType::Freelist, reference, 4096).expect("freelist frame");
        let mut trunk = Freelist::empty_trunk(next);
        for page_id in leaves {
            Freelist::release(&mut trunk, *page_id, 4096).expect("freelist leaf");
        }
        Freelist::write(&mut frame, &trunk).expect("encode freelist trunk");
        frame.prepare_for_write().expect("stamp freelist frame");
        frame
    }

    struct TestSource {
        page_count: u64,
        freelist_root: PageId,
        pages: BTreeMap<PageId, Vec<u8>>,
    }

    impl TestSource {
        fn new(page_count: u64, freelist_root: PageId) -> Self {
            Self {
                page_count,
                freelist_root,
                pages: BTreeMap::new(),
            }
        }

        fn put(&mut self, frame: PageFrame) {
            self.pages
                .insert(frame.header().page_id(), frame.bytes().to_vec());
        }
    }

    struct IoSource;

    impl PageSource for IoSource {
        fn read_page(&mut self, _reference: PageRef) -> Result<PageHandle> {
            Err(DbError::new(ErrorCode::Io, "injected page-source I/O"))
        }

        fn page_count(&self) -> Result<u64> {
            Ok(2)
        }
    }

    impl PageSource for TestSource {
        fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
            let bytes = self
                .pages
                .get(&reference.page_id())
                .ok_or_else(|| DbError::new(ErrorCode::Corruption, "test page not found"))?
                .clone();
            let frame = PageFrame::decode(
                bytes,
                reference.page_id(),
                None,
                Some(reference.generation()),
            )?;
            Ok(Arc::new(frame))
        }

        fn read_page_by_id(&mut self, page_id: PageId) -> Result<PageHandle> {
            let bytes = self
                .pages
                .get(&page_id)
                .ok_or_else(|| DbError::new(ErrorCode::Corruption, "test page not found"))?
                .clone();
            let frame = PageFrame::decode(bytes, page_id, None, None)?;
            Ok(Arc::new(frame))
        }

        fn page_count(&self) -> Result<u64> {
            Ok(self.page_count)
        }

        fn freelist_root(&self) -> Result<PageId> {
            Ok(self.freelist_root)
        }
    }
}
