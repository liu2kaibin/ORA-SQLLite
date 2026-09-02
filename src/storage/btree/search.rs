use std::{collections::BTreeSet, ops::Bound};

use super::{
    INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, LeafCell, TreeKind, ValueStorage,
    decode_internal_leftmost, decode_leaf_right_sibling, validate_key,
};
use crate::storage::{
    PageId, PageRef, overflow,
    pager::{PageSource, Pager, WriteBatch},
    slotted::SlottedPage,
};
use crate::{DbError, ErrorCode, Result};

pub(in crate::storage) struct BTree {
    kind: TreeKind,
    root: PageRef,
}

struct InternalNode {
    leftmost: PageRef,
    cells: Vec<InternalCell>,
}

struct LeafNode {
    cells: Vec<LeafCell>,
    right_sibling: Option<PageRef>,
}

enum Node {
    Leaf(LeafNode),
    Internal(InternalNode),
}

impl InternalNode {
    fn child_for(&self, key: &[u8]) -> PageRef {
        let mut lower = 0usize;
        let mut upper = self.cells.len();
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            if self.cells[middle].key() <= key {
                lower = middle + 1;
            } else {
                upper = middle;
            }
        }
        if lower == 0 {
            self.leftmost
        } else {
            self.cells[lower - 1].right()
        }
    }
}

impl BTree {
    pub(in crate::storage) fn create(batch: &mut WriteBatch<'_>, kind: TreeKind) -> Result<Self> {
        let root = batch.allocate(kind.leaf_page_type())?;
        let frame = batch.modify(root)?;
        SlottedPage::open(frame, LEAF_METADATA_END)?;
        Ok(Self { kind, root })
    }

    pub(in crate::storage) const fn root(&self) -> PageRef {
        self.root
    }

    pub(in crate::storage) fn get(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        validate_key(key)?;
        let (_, leaf) = descend_to_leaf(pager, self.kind, self.root, key)?;
        let index = lower_bound(&leaf.cells, key);
        let Some(cell) = leaf.cells.get(index).filter(|cell| cell.key() == key) else {
            return Ok(None);
        };
        read_value(pager, cell).map(Some)
    }

    pub(in crate::storage) fn scan<'a>(
        &self,
        pager: &'a mut Pager,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<BTreeCursor<'a>> {
        validate_bound(&start)?;
        validate_bound(&end)?;
        let (current, _leaf, next_slot) = match start {
            Bound::Unbounded => {
                let (reference, leaf) = descend_to_leftmost(pager, self.kind, self.root)?;
                (reference, leaf, 0)
            }
            Bound::Included(key) => {
                let (reference, leaf) = descend_to_leaf(pager, self.kind, self.root, key)?;
                let next_slot = lower_bound(&leaf.cells, key);
                (reference, leaf, next_slot)
            }
            Bound::Excluded(key) => {
                let (reference, leaf) = descend_to_leaf(pager, self.kind, self.root, key)?;
                let next_slot = upper_bound(&leaf.cells, key);
                (reference, leaf, next_slot)
            }
        };
        let max_leaf_hops = max_page_hops(pager)?;
        let mut visited = BTreeSet::new();
        visit_page(&mut visited, current, max_leaf_hops, "B+Tree leaf chain")?;
        Ok(BTreeCursor {
            pager,
            kind: self.kind,
            current: Some(current),
            next_slot,
            end: own_bound(end),
            visited,
            max_leaf_hops,
            previous_leaf_last_key: None,
        })
    }
}

fn descend_to_leaf(
    source: &mut impl PageSource,
    kind: TreeKind,
    root: PageRef,
    key: &[u8],
) -> Result<(PageRef, LeafNode)> {
    descend_to_leaf_by(source, kind, root, |internal| internal.child_for(key))
}

fn descend_to_leftmost(
    source: &mut impl PageSource,
    kind: TreeKind,
    root: PageRef,
) -> Result<(PageRef, LeafNode)> {
    descend_to_leaf_by(source, kind, root, |internal| internal.leftmost)
}

fn descend_to_leaf_by(
    source: &mut impl PageSource,
    kind: TreeKind,
    root: PageRef,
    mut select_child: impl FnMut(&InternalNode) -> PageRef,
) -> Result<(PageRef, LeafNode)> {
    let max_hops = max_page_hops(source)?;
    let mut visited = BTreeSet::new();
    let mut current = root;

    loop {
        visit_page(&mut visited, current, max_hops, "B+Tree descent")?;
        match load_node(source, kind, current)? {
            Node::Leaf(leaf) => return Ok((current, leaf)),
            Node::Internal(internal) => current = select_child(&internal),
        }
    }
}

fn load_node(source: &mut impl PageSource, kind: TreeKind, reference: PageRef) -> Result<Node> {
    let handle = source.read_page(reference)?;
    let mut frame = handle.borrow_mut();
    if frame.header().page_id() != reference.page_id()
        || frame.header().generation() != reference.generation()
    {
        return Err(corruption(
            "B+Tree page reference does not match the loaded page",
        ));
    }

    match frame.header().page_type() {
        page_type if page_type == kind.leaf_page_type() => {
            reject_uninitialized_btree_layout(&frame)?;
            let right_sibling = decode_leaf_right_sibling(
                &frame.bytes()[LEAF_METADATA_END - 8..LEAF_METADATA_END],
                frame.header().right_sibling(),
            )?;
            let cells = SlottedPage::open(&mut frame, LEAF_METADATA_END)?.cells()?;
            let cells = decode_leaf_cells(cells)?;
            Ok(Node::Leaf(LeafNode {
                cells,
                right_sibling,
            }))
        }
        page_type if page_type == kind.internal_page_type() => {
            reject_uninitialized_btree_layout(&frame)?;
            let leftmost = decode_internal_leftmost(
                &frame.bytes()[INTERNAL_METADATA_END - 16..INTERNAL_METADATA_END],
            )?;
            let cells = SlottedPage::open(&mut frame, INTERNAL_METADATA_END)?.cells()?;
            let cells = decode_internal_cells(cells)?;
            Ok(Node::Internal(InternalNode { leftmost, cells }))
        }
        _ => Err(corruption("B+Tree node has an unexpected page type")),
    }
}

fn decode_leaf_cells(cells: Vec<Vec<u8>>) -> Result<Vec<LeafCell>> {
    let cells = cells
        .into_iter()
        .map(|cell| LeafCell::decode(&cell))
        .collect::<Result<Vec<_>>>()?;
    require_strict_leaf_keys(&cells)?;
    Ok(cells)
}

fn decode_internal_cells(cells: Vec<Vec<u8>>) -> Result<Vec<InternalCell>> {
    let cells = cells
        .into_iter()
        .map(|cell| InternalCell::decode(&cell))
        .collect::<Result<Vec<_>>>()?;
    require_strict_internal_keys(&cells)?;
    Ok(cells)
}

fn require_strict_leaf_keys(cells: &[LeafCell]) -> Result<()> {
    for pair in cells.windows(2) {
        if pair[0].key() >= pair[1].key() {
            return Err(corruption("B+Tree leaf keys are not strictly ordered"));
        }
    }
    Ok(())
}

fn require_strict_internal_keys(cells: &[InternalCell]) -> Result<()> {
    for pair in cells.windows(2) {
        if pair[0].key() >= pair[1].key() {
            return Err(corruption("B+Tree internal keys are not strictly ordered"));
        }
    }
    Ok(())
}

fn reject_uninitialized_btree_layout(frame: &crate::storage::page::PageFrame) -> Result<()> {
    if frame.header().cell_count() == 0
        && frame.header().slot_end() == 48
        && usize::from(frame.header().payload_start()) == frame.bytes().len()
    {
        return Err(corruption(
            "B+Tree node has an uninitialized slotted layout",
        ));
    }
    Ok(())
}

fn lower_bound(cells: &[LeafCell], key: &[u8]) -> usize {
    let mut lower = 0usize;
    let mut upper = cells.len();
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        if cells[middle].key() < key {
            lower = middle + 1;
        } else {
            upper = middle;
        }
    }
    lower
}

fn upper_bound(cells: &[LeafCell], key: &[u8]) -> usize {
    let mut lower = 0usize;
    let mut upper = cells.len();
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        if cells[middle].key() <= key {
            lower = middle + 1;
        } else {
            upper = middle;
        }
    }
    lower
}

fn validate_bound(bound: &Bound<&[u8]>) -> Result<()> {
    match bound {
        Bound::Included(key) | Bound::Excluded(key) => validate_key(key),
        Bound::Unbounded => Ok(()),
    }
}

fn read_value(pager: &mut Pager, cell: &LeafCell) -> Result<Vec<u8>> {
    match cell.storage() {
        ValueStorage::Inline(value) => Ok(value.clone()),
        ValueStorage::Overflow(first) => {
            overflow::read_value(pager, *first, cell.total_value_len())
        }
    }
}

fn max_page_hops(source: &impl PageSource) -> Result<u64> {
    source
        .page_count()?
        .checked_sub(1)
        .ok_or_else(|| corruption("database has no header page"))
}

fn visit_page(
    visited: &mut BTreeSet<PageId>,
    reference: PageRef,
    max_hops: u64,
    context: &'static str,
) -> Result<()> {
    let visited_count = u64::try_from(visited.len())
        .map_err(|_| corruption("B+Tree visited-page count exceeds u64"))?;
    if visited_count >= max_hops {
        return Err(corruption(match context {
            "B+Tree descent" => "B+Tree descent exceeds the ordinary page count",
            "B+Tree leaf chain" => "B+Tree leaf chain exceeds the ordinary page count",
            _ => "B+Tree traversal exceeds the ordinary page count",
        }));
    }
    if !visited.insert(reference.page_id()) {
        return Err(corruption(match context {
            "B+Tree descent" => "B+Tree descent has a cycle",
            "B+Tree leaf chain" => "B+Tree leaf chain has a cycle",
            _ => "B+Tree traversal has a cycle",
        }));
    }
    Ok(())
}

pub(in crate::storage) struct BTreeCursor<'a> {
    pager: &'a mut Pager,
    kind: TreeKind,
    current: Option<PageRef>,
    next_slot: usize,
    end: Bound<Vec<u8>>,
    visited: BTreeSet<PageId>,
    max_leaf_hops: u64,
    previous_leaf_last_key: Option<Vec<u8>>,
}

impl BTreeCursor<'_> {
    pub(in crate::storage) fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            let Some(reference) = self.current else {
                return Ok(None);
            };
            let leaf = load_leaf(self.pager, self.kind, reference)?;
            if let Some(previous) = self.previous_leaf_last_key.as_deref()
                && leaf
                    .cells
                    .first()
                    .is_some_and(|cell| cell.key() <= previous)
            {
                return Err(corruption(
                    "B+Tree leaf chain keys are not strictly ordered",
                ));
            }

            if let Some(cell) = leaf.cells.get(self.next_slot) {
                self.next_slot = self
                    .next_slot
                    .checked_add(1)
                    .ok_or_else(|| corruption("B+Tree cursor slot index overflows"))?;
                if is_past_end(cell.key(), &self.end) {
                    self.current = None;
                    return Ok(None);
                }
                return read_value(self.pager, cell)
                    .map(|value| Some((cell.key().to_vec(), value)));
            }

            if let Some(last) = leaf.cells.last() {
                self.previous_leaf_last_key = Some(last.key().to_vec());
            }
            match leaf.right_sibling {
                Some(right) => {
                    visit_page(
                        &mut self.visited,
                        right,
                        self.max_leaf_hops,
                        "B+Tree leaf chain",
                    )?;
                    self.current = Some(right);
                    self.next_slot = 0;
                }
                None => {
                    self.current = None;
                    return Ok(None);
                }
            }
        }
    }
}

fn load_leaf(source: &mut impl PageSource, kind: TreeKind, reference: PageRef) -> Result<LeafNode> {
    match load_node(source, kind, reference)? {
        Node::Leaf(leaf) => Ok(leaf),
        Node::Internal(_) => Err(corruption("B+Tree leaf chain points to an internal page")),
    }
}

fn is_past_end(key: &[u8], end: &Bound<Vec<u8>>) -> bool {
    match end {
        Bound::Included(end) => key > end,
        Bound::Excluded(end) => key >= end,
        Bound::Unbounded => false,
    }
}

fn own_bound(bound: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Included(value) => Bound::Included(value.to_vec()),
        Bound::Excluded(value) => Bound::Excluded(value.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::InternalNode;
    use crate::ErrorCode;
    use crate::storage::{
        PageId, PageRef,
        btree::{BTree, InternalCell, LeafCell, TreeKind, encode_leaf_right_sibling},
        overflow,
        page::PageType,
        pager::{Pager, WriteBatch},
        slotted::SlottedPage,
        test_support::test_pager,
    };

    #[test]
    fn internal_routing_uses_last_separator_not_greater_than_key() {
        let node = InternalNode {
            leftmost: page_ref(1, 1),
            cells: vec![
                InternalCell::new(b"m".to_vec(), page_ref(2, 1)).expect("first separator"),
                InternalCell::new(b"t".to_vec(), page_ref(3, 1)).expect("second separator"),
            ],
        };

        assert_eq!(node.child_for(b"a"), page_ref(1, 1));
        assert_eq!(node.child_for(b"m"), page_ref(2, 1));
        assert_eq!(node.child_for(b"z"), page_ref(3, 1));
    }

    #[test]
    fn point_lookup_returns_the_exact_inline_leaf_value() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"ant", b"one"), (b"bee", b"two")]);
            batch.commit().expect("commit leaf");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };

        assert_eq!(
            tree.get(&mut pager, b"bee").expect("point lookup"),
            Some(b"two".to_vec())
        );
        assert_eq!(tree.get(&mut pager, b"cat").expect("missing key"), None);
    }

    #[test]
    fn point_lookup_rejects_keys_longer_than_the_disk_limit() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin write");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("empty tree");
            batch.commit().expect("commit empty tree");
            tree
        };

        assert_eq!(
            tree.get(&mut pager, &[0; 1025])
                .expect_err("keys over 1024 bytes are invalid")
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn range_scan_applies_inclusive_start_and_exclusive_end_across_leaf_pages() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = three_leaf_tree(&mut pager);

        assert_eq!(
            collect_scan(
                &tree,
                &mut pager,
                Bound::Included(b"bee"),
                Bound::Excluded(b"eel"),
            ),
            vec![
                (b"bee".to_vec(), b"two".to_vec()),
                (b"cat".to_vec(), b"three".to_vec()),
                (b"dog".to_vec(), b"four".to_vec()),
            ]
        );
    }

    #[test]
    fn range_scan_supports_unbounded_start_and_inclusive_end() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = three_leaf_tree(&mut pager);

        assert_eq!(
            collect_scan(&tree, &mut pager, Bound::Unbounded, Bound::Included(b"cat"),),
            vec![
                (b"ant".to_vec(), b"one".to_vec()),
                (b"bee".to_vec(), b"two".to_vec()),
                (b"cat".to_vec(), b"three".to_vec()),
            ]
        );
    }

    #[test]
    fn point_lookup_descends_three_levels_when_a_separator_equals_the_key() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = three_level_tree(&mut pager);

        assert_eq!(
            tree.get(&mut pager, b"m").expect("equal separator lookup"),
            Some(b"middle".to_vec())
        );
        assert_eq!(
            tree.get(&mut pager, b"ant").expect("leftmost lookup"),
            Some(b"one".to_vec())
        );
    }

    #[test]
    fn range_scan_descends_to_the_leftmost_leaf_of_a_three_level_tree() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = three_level_tree(&mut pager);

        assert_eq!(
            collect_scan(&tree, &mut pager, Bound::Unbounded, Bound::Unbounded),
            vec![
                (b"ant".to_vec(), b"one".to_vec()),
                (b"bee".to_vec(), b"two".to_vec()),
                (b"m".to_vec(), b"middle".to_vec()),
                (b"zoo".to_vec(), b"last".to_vec()),
            ]
        );
    }

    #[test]
    fn point_lookup_reads_an_overflow_value_from_the_pager_chain() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let value = vec![0x5a; 4097];
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let first = overflow::write_value(&mut batch, &value)
                .expect("write overflow chain")
                .expect("nonempty overflow chain");
            let root = write_leaf_cell(
                &mut batch,
                LeafCell::overflow(b"blob".to_vec(), value.len() as u64, first)
                    .expect("overflow leaf cell"),
            );
            batch.commit().expect("commit overflow tree");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };

        assert_eq!(
            tree.get(&mut pager, b"blob").expect("overflow lookup"),
            Some(value)
        );
    }

    #[test]
    fn point_lookup_rejects_a_cycle_in_internal_links() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = batch
                .allocate(PageType::TableInternal)
                .expect("allocate internal root");
            let frame = batch.modify(root).expect("modify internal root");
            frame.bytes_mut()[48..64]
                .copy_from_slice(&crate::storage::btree::encode_internal_leftmost(root));
            SlottedPage::open(frame, 64).expect("open internal root");
            batch.commit().expect("commit cyclic root");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };

        assert_eq!(
            tree.get(&mut pager, b"key")
                .expect_err("cyclic internal links must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn point_lookup_rejects_leaf_cells_that_are_not_strictly_ordered() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"bee", b"two"), (b"ant", b"one")]);
            batch.commit().expect("commit unordered leaf");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };

        assert_eq!(
            tree.get(&mut pager, b"ant")
                .expect_err("unordered leaf keys must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn point_lookup_rejects_internal_cells_that_are_not_strictly_ordered() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let left = write_leaf(&mut batch, &[(b"ant", b"one")]);
            let right = write_leaf(&mut batch, &[(b"zoo", b"last")]);
            let root = write_internal(&mut batch, left, &[(b"t", right), (b"m", right)]);
            batch.commit().expect("commit unordered internal");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };

        assert_eq!(
            tree.get(&mut pager, b"m")
                .expect_err("unordered internal keys must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn point_lookup_rejects_a_root_with_the_wrong_tree_kind_page_type() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = batch
                .allocate(PageType::IndexLeaf)
                .expect("allocate index leaf page");
            let frame = batch.modify(root).expect("modify index leaf page");
            SlottedPage::open(frame, 56).expect("open index leaf page");
            batch.commit().expect("commit index leaf");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };

        assert_eq!(
            tree.get(&mut pager, b"key")
                .expect_err("wrong B+Tree page type must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scan_rejects_a_leaf_sibling_cycle() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"ant", b"one")]);
            link_right_sibling(&mut batch, root, root);
            batch.commit().expect("commit cyclic leaf chain");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };
        let mut cursor = tree
            .scan(&mut pager, Bound::Unbounded, Bound::Unbounded)
            .expect("start scan");

        assert_eq!(
            cursor.next().expect("first leaf row"),
            Some((b"ant".to_vec(), b"one".to_vec()))
        );
        assert_eq!(
            cursor
                .next()
                .expect_err("cyclic leaf chain must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scan_rejects_a_non_leaf_sibling_target() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"ant", b"one")]);
            let internal = write_internal(&mut batch, root, &[]);
            link_right_sibling(&mut batch, root, internal);
            batch.commit().expect("commit invalid leaf chain");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };
        let mut cursor = tree
            .scan(&mut pager, Bound::Unbounded, Bound::Unbounded)
            .expect("start scan");

        assert!(cursor.next().expect("first leaf row").is_some());
        assert_eq!(
            cursor
                .next()
                .expect_err("internal sibling target must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scan_rejects_a_sibling_generation_mismatch() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"ant", b"one")]);
            let right = write_leaf(&mut batch, &[(b"bee", b"two")]);
            let wrong_generation = PageRef::new(right.page_id(), right.generation() + 1)
                .expect("different valid generation");
            link_right_sibling(&mut batch, root, wrong_generation);
            batch.commit().expect("commit invalid generation link");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };
        let mut cursor = tree
            .scan(&mut pager, Bound::Unbounded, Bound::Unbounded)
            .expect("start scan");

        assert!(cursor.next().expect("first leaf row").is_some());
        assert_eq!(
            cursor
                .next()
                .expect_err("generation mismatch must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scan_rejects_leaf_chains_with_decreasing_first_keys() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"zoo", b"last")]);
            let right = write_leaf(&mut batch, &[(b"ant", b"first")]);
            link_right_sibling(&mut batch, root, right);
            batch.commit().expect("commit unordered leaf chain");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };
        let mut cursor = tree
            .scan(&mut pager, Bound::Unbounded, Bound::Unbounded)
            .expect("start scan");

        assert!(cursor.next().expect("first leaf row").is_some());
        assert_eq!(
            cursor
                .next()
                .expect_err("decreasing leaf keys must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scan_rejects_leaf_chains_with_duplicate_boundary_keys() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = write_leaf(&mut batch, &[(b"bee", b"left")]);
            let right = write_leaf(&mut batch, &[(b"bee", b"right")]);
            link_right_sibling(&mut batch, root, right);
            batch.commit().expect("commit duplicate leaf chain");
            root
        };
        let tree = BTree {
            kind: TreeKind::Table,
            root,
        };
        let mut cursor = tree
            .scan(&mut pager, Bound::Unbounded, Bound::Unbounded)
            .expect("start scan");

        assert_eq!(
            cursor.next().expect("first leaf row"),
            Some((b"bee".to_vec(), b"left".to_vec()))
        );
        assert_eq!(
            cursor
                .next()
                .expect_err("duplicate boundary key must fail")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scan_rejects_bounds_with_keys_longer_than_the_disk_limit() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin write");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("empty tree");
            batch.commit().expect("commit empty tree");
            tree
        };

        let error = match tree.scan(&mut pager, Bound::Included(&[0; 1025]), Bound::Unbounded) {
            Ok(_) => panic!("oversized start key must fail"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Type);
    }

    fn page_ref(page_id: u64, generation: u64) -> PageRef {
        PageRef::new(PageId(page_id), generation).expect("page reference")
    }

    fn write_leaf(batch: &mut WriteBatch<'_>, cells: &[(&[u8], &[u8])]) -> PageRef {
        let reference = batch
            .allocate(PageType::TableLeaf)
            .expect("allocate leaf page");
        let frame = batch.modify(reference).expect("modify leaf page");
        let mut page = SlottedPage::open(frame, 56).expect("open leaf page");
        for (index, (key, value)) in cells.iter().enumerate() {
            let cell = LeafCell::inline(key.to_vec(), value.to_vec()).expect("leaf cell");
            page.insert(index, &cell.encode().expect("encode leaf cell"))
                .expect("insert leaf cell");
        }
        reference
    }

    fn write_leaf_cell(batch: &mut WriteBatch<'_>, cell: LeafCell) -> PageRef {
        let reference = batch
            .allocate(PageType::TableLeaf)
            .expect("allocate leaf page");
        let frame = batch.modify(reference).expect("modify leaf page");
        let mut page = SlottedPage::open(frame, 56).expect("open leaf page");
        page.insert(0, &cell.encode().expect("encode leaf cell"))
            .expect("insert leaf cell");
        reference
    }

    fn write_internal(
        batch: &mut WriteBatch<'_>,
        leftmost: PageRef,
        cells: &[(&[u8], PageRef)],
    ) -> PageRef {
        let reference = batch
            .allocate(PageType::TableInternal)
            .expect("allocate internal page");
        let frame = batch.modify(reference).expect("modify internal page");
        frame.bytes_mut()[48..64]
            .copy_from_slice(&crate::storage::btree::encode_internal_leftmost(leftmost));
        let mut page = SlottedPage::open(frame, 64).expect("open internal page");
        for (index, (key, right)) in cells.iter().enumerate() {
            let cell = InternalCell::new(key.to_vec(), *right).expect("internal cell");
            page.insert(index, &cell.encode().expect("encode internal cell"))
                .expect("insert internal cell");
        }
        reference
    }

    fn three_leaf_tree(pager: &mut Pager) -> BTree {
        let left = {
            let mut batch = pager.begin_write().expect("begin write");
            let left = write_leaf(&mut batch, &[(b"ant", b"one"), (b"bee", b"two")]);
            let middle = write_leaf(&mut batch, &[(b"cat", b"three"), (b"dog", b"four")]);
            let right = write_leaf(&mut batch, &[(b"eel", b"five")]);
            link_right_sibling(&mut batch, left, middle);
            link_right_sibling(&mut batch, middle, right);
            batch.commit().expect("commit leaf chain");
            left
        };
        BTree {
            kind: TreeKind::Table,
            root: left,
        }
    }

    fn three_level_tree(pager: &mut Pager) -> BTree {
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let left_leaf = write_leaf(&mut batch, &[(b"ant", b"one"), (b"bee", b"two")]);
            let right_leaf = write_leaf(&mut batch, &[(b"m", b"middle"), (b"zoo", b"last")]);
            link_right_sibling(&mut batch, left_leaf, right_leaf);
            let left_internal = write_internal(&mut batch, left_leaf, &[]);
            let right_internal = write_internal(&mut batch, right_leaf, &[]);
            let root = write_internal(&mut batch, left_internal, &[(b"m", right_internal)]);
            batch.commit().expect("commit three-level tree");
            root
        };
        BTree {
            kind: TreeKind::Table,
            root,
        }
    }

    fn link_right_sibling(batch: &mut WriteBatch<'_>, leaf: PageRef, right: PageRef) {
        let frame = batch.modify(leaf).expect("modify leaf page");
        frame
            .set_right_sibling(Some(right.page_id()))
            .expect("set right sibling page id");
        frame.bytes_mut()[48..56].copy_from_slice(&encode_leaf_right_sibling(Some(right)));
    }

    fn collect_scan(
        tree: &BTree,
        pager: &mut Pager,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut cursor = tree.scan(pager, start, end).expect("start scan");
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().expect("read scan row") {
            rows.push(row);
        }
        rows
    }
}
