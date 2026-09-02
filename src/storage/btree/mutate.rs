use std::collections::HashSet;

use super::{
    INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, LeafCell, encode_internal_leftmost,
    encode_leaf_right_sibling,
    search::{BTree, InternalNode, LeafNode, Node, load_node, lower_bound},
    validate_key,
};
use crate::storage::{
    PageRef, overflow,
    pager::{PageSource, WriteBatch},
    slotted::SlottedPage,
};
use crate::{DbError, ErrorCode, Result};

const SLOT_LEN: usize = 4;
type SearchPath = Vec<(PageRef, usize)>;
type LeafDescent = (PageRef, LeafNode, SearchPath);

struct Split {
    separator: Vec<u8>,
    right: PageRef,
}

impl BTree {
    pub(in crate::storage) fn insert(
        &mut self,
        batch: &mut WriteBatch<'_>,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        validate_key(key)?;
        let old_root = self.root;
        let checkpoint = batch.checkpoint_mutation()?;
        let result = self
            .insert_inner(batch, key, value)
            .and_then(|()| batch.refresh_tree(self.kind, old_root, self.root));
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.root = old_root;
                batch.restore_mutation(checkpoint);
                Err(error)
            }
        }
    }

    fn insert_inner(&mut self, batch: &mut WriteBatch<'_>, key: &[u8], value: &[u8]) -> Result<()> {
        let (leaf_ref, mut leaf, mut path) = descend_with_path(batch, self.kind, self.root, key)?;
        let insert_at = lower_bound(&leaf.cells, key);
        if leaf
            .cells
            .get(insert_at)
            .is_some_and(|cell| cell.key() == key)
        {
            return Err(DbError::new(
                ErrorCode::Constraint,
                "duplicate physical B+Tree key",
            ));
        }

        let inline = LeafCell::inline(key.to_vec(), value.to_vec())?;
        let inline_bytes = inline.encode()?;
        let cell = if value.is_empty()
            || cell_fits_target(batch, leaf_ref, LEAF_METADATA_END, inline_bytes.len())?
        {
            inline
        } else {
            let total_value_len = u64::try_from(value.len())
                .map_err(|_| corruption("B+Tree value length exceeds u64"))?;
            let first = overflow::write_value(batch, value)?
                .ok_or_else(|| corruption("nonempty overflow value allocated no pages"))?;
            LeafCell::overflow(key.to_vec(), total_value_len, first)?
        };
        leaf.cells.insert(insert_at, cell);

        let mut split = write_or_split_leaf(batch, self.kind, leaf_ref, leaf)?;
        while let Some(child_split) = split {
            let Some((parent_ref, child_index)) = path.pop() else {
                let new_root = batch.allocate(self.kind.internal_page_type())?;
                rewrite_internal(
                    batch,
                    new_root,
                    self.root,
                    &[InternalCell::new(child_split.separator, child_split.right)?],
                )?;
                self.root = new_root;
                break;
            };

            let mut parent = match load_node(batch, self.kind, parent_ref)? {
                Node::Internal(parent) => parent,
                Node::Leaf(_) => return Err(corruption("B+Tree path parent is a leaf page")),
            };
            if child_index > parent.cells.len() {
                return Err(corruption("B+Tree path child index is outside its parent"));
            }
            parent.cells.insert(
                child_index,
                InternalCell::new(child_split.separator, child_split.right)?,
            );
            split = write_or_split_internal(batch, self.kind, parent_ref, parent)?;
        }

        Ok(())
    }
}

fn descend_with_path(
    source: &mut impl PageSource,
    kind: super::TreeKind,
    root: PageRef,
    key: &[u8],
) -> Result<LeafDescent> {
    let max_hops = source.page_count()?;
    let mut visited = HashSet::new();
    let mut path = Vec::new();
    let mut current = root;
    loop {
        if u64::try_from(visited.len()).map_or(true, |count| count >= max_hops)
            || !visited.insert(current)
        {
            return Err(corruption("B+Tree insertion descent has a cycle"));
        }
        match load_node(source, kind, current)? {
            Node::Leaf(leaf) => return Ok((current, leaf, path)),
            Node::Internal(internal) => {
                let child_index = internal.cells.partition_point(|cell| cell.key() <= key);
                let child = if child_index == 0 {
                    internal.leftmost
                } else {
                    internal.cells[child_index - 1].right()
                };
                path.push((current, child_index));
                current = child;
            }
        }
    }
}

fn cell_fits_target(
    batch: &mut WriteBatch<'_>,
    reference: PageRef,
    metadata_end: usize,
    encoded_len: usize,
) -> Result<bool> {
    let handle = batch.read(reference)?;
    let mut frame = handle.borrow_mut();
    let free = SlottedPage::open(&mut frame, metadata_end)?.compacted_free()?;
    Ok(encoded_len
        .checked_add(SLOT_LEN)
        .is_some_and(|required| required <= free))
}

fn write_or_split_leaf(
    batch: &mut WriteBatch<'_>,
    kind: super::TreeKind,
    left_ref: PageRef,
    leaf: LeafNode,
) -> Result<Option<Split>> {
    let encoded = encode_leaf_cells(&leaf.cells)?;
    let page_len = batch.read(left_ref)?.borrow().bytes().len();
    if encoded_cells_fit(page_len, LEAF_METADATA_END, &encoded)? {
        rewrite_leaf(batch, left_ref, leaf.right_sibling, &leaf.cells)?;
        return Ok(None);
    }

    let split_at = balanced_leaf_boundary(page_len, &encoded)?;
    let right_ref = batch.allocate(kind.leaf_page_type())?;
    let (left_cells, right_cells) = leaf.cells.split_at(split_at);
    let separator = right_cells
        .first()
        .ok_or_else(|| corruption("leaf split produced an empty right page"))?
        .key()
        .to_vec();
    rewrite_leaf(batch, right_ref, leaf.right_sibling, right_cells)?;
    rewrite_leaf(batch, left_ref, Some(right_ref), left_cells)?;
    Ok(Some(Split {
        separator,
        right: right_ref,
    }))
}

fn write_or_split_internal(
    batch: &mut WriteBatch<'_>,
    kind: super::TreeKind,
    left_ref: PageRef,
    node: InternalNode,
) -> Result<Option<Split>> {
    let encoded = encode_internal_cells(&node.cells)?;
    let page_len = batch.read(left_ref)?.borrow().bytes().len();
    if encoded_cells_fit(page_len, INTERNAL_METADATA_END, &encoded)? {
        rewrite_internal(batch, left_ref, node.leftmost, &node.cells)?;
        return Ok(None);
    }

    let promote_at = balanced_internal_separator(page_len, &encoded)?;
    let promoted = node
        .cells
        .get(promote_at)
        .ok_or_else(|| corruption("internal split separator is outside the page"))?;
    let right_ref = batch.allocate(kind.internal_page_type())?;
    rewrite_internal(
        batch,
        right_ref,
        promoted.right(),
        &node.cells[promote_at + 1..],
    )?;
    rewrite_internal(batch, left_ref, node.leftmost, &node.cells[..promote_at])?;
    Ok(Some(Split {
        separator: promoted.key().to_vec(),
        right: right_ref,
    }))
}

fn balanced_leaf_boundary(page_len: usize, encoded: &[Vec<u8>]) -> Result<usize> {
    let mut best: Option<(usize, usize)> = None;
    for split_at in 1..encoded.len() {
        let left = &encoded[..split_at];
        let right = &encoded[split_at..];
        if !encoded_cells_fit(page_len, LEAF_METADATA_END, left)?
            || !encoded_cells_fit(page_len, LEAF_METADATA_END, right)?
        {
            continue;
        }
        let difference = encoded_bytes(left)?.abs_diff(encoded_bytes(right)?);
        if best.is_none_or(|(best_difference, _)| difference < best_difference) {
            best = Some((difference, split_at));
        }
    }
    best.map(|(_, split_at)| split_at).ok_or_else(|| {
        DbError::new(
            ErrorCode::Type,
            "leaf cells cannot be split across two pages",
        )
    })
}

fn balanced_internal_separator(page_len: usize, encoded: &[Vec<u8>]) -> Result<usize> {
    let mut best: Option<(usize, usize)> = None;
    for promote_at in 1..encoded.len().saturating_sub(1) {
        let left = &encoded[..promote_at];
        let right = &encoded[promote_at + 1..];
        if !encoded_cells_fit(page_len, INTERNAL_METADATA_END, left)?
            || !encoded_cells_fit(page_len, INTERNAL_METADATA_END, right)?
        {
            continue;
        }
        let difference = encoded_bytes(left)?.abs_diff(encoded_bytes(right)?);
        if best.is_none_or(|(best_difference, _)| difference < best_difference) {
            best = Some((difference, promote_at));
        }
    }
    best.map(|(_, promote_at)| promote_at).ok_or_else(|| {
        DbError::new(
            ErrorCode::Type,
            "internal cells cannot be split across two pages",
        )
    })
}

fn encoded_cells_fit(page_len: usize, metadata_end: usize, cells: &[Vec<u8>]) -> Result<bool> {
    let used = metadata_end
        .checked_add(encoded_payload_bytes(cells)?)
        .and_then(|used| used.checked_add(cells.len().checked_mul(SLOT_LEN)?))
        .ok_or_else(|| corruption("B+Tree page byte accounting overflow"))?;
    Ok(used <= page_len)
}

fn encoded_bytes(cells: &[Vec<u8>]) -> Result<usize> {
    cells.iter().try_fold(0usize, |total, cell| {
        total
            .checked_add(cell.len() + SLOT_LEN)
            .ok_or_else(|| corruption("B+Tree encoded cell bytes overflow"))
    })
}

fn encoded_payload_bytes(cells: &[Vec<u8>]) -> Result<usize> {
    cells.iter().try_fold(0usize, |total, cell| {
        total
            .checked_add(cell.len())
            .ok_or_else(|| corruption("B+Tree encoded payload bytes overflow"))
    })
}

fn encode_leaf_cells(cells: &[LeafCell]) -> Result<Vec<Vec<u8>>> {
    cells.iter().map(LeafCell::encode).collect()
}

fn encode_internal_cells(cells: &[InternalCell]) -> Result<Vec<Vec<u8>>> {
    cells.iter().map(InternalCell::encode).collect()
}

fn rewrite_leaf(
    batch: &mut WriteBatch<'_>,
    reference: PageRef,
    right_sibling: Option<PageRef>,
    cells: &[LeafCell],
) -> Result<()> {
    let frame = batch.modify(reference)?;
    reset_slotted(frame, LEAF_METADATA_END)?;
    frame.set_right_sibling(right_sibling.map(PageRef::page_id))?;
    frame.bytes_mut()[LEAF_METADATA_END - 8..LEAF_METADATA_END]
        .copy_from_slice(&encode_leaf_right_sibling(right_sibling));
    let mut page = SlottedPage::open(frame, LEAF_METADATA_END)?;
    for (index, cell) in cells.iter().enumerate() {
        page.insert(index, &cell.encode()?)?;
    }
    page.validate()
}

fn rewrite_internal(
    batch: &mut WriteBatch<'_>,
    reference: PageRef,
    leftmost: PageRef,
    cells: &[InternalCell],
) -> Result<()> {
    let frame = batch.modify(reference)?;
    reset_slotted(frame, INTERNAL_METADATA_END)?;
    frame.set_right_sibling(None)?;
    frame.bytes_mut()[INTERNAL_METADATA_END - 16..INTERNAL_METADATA_END]
        .copy_from_slice(&encode_internal_leftmost(leftmost));
    let mut page = SlottedPage::open(frame, INTERNAL_METADATA_END)?;
    for (index, cell) in cells.iter().enumerate() {
        page.insert(index, &cell.encode()?)?;
    }
    page.validate()
}

fn reset_slotted(frame: &mut crate::storage::page::PageFrame, metadata_end: usize) -> Result<()> {
    let page_len = frame.bytes().len();
    frame.bytes_mut()[48..].fill(0);
    frame.set_layout(
        0,
        u16::try_from(metadata_end).map_err(|_| corruption("B+Tree metadata end exceeds u16"))?,
        u16::try_from(page_len).map_err(|_| corruption("B+Tree page length exceeds u16"))?,
    )
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, ops::Bound};

    use super::{
        super::{TreeKind, ValueStorage, search::BTree},
        Node, PageSource, load_node, lower_bound,
    };
    use crate::{
        ErrorCode, Result,
        storage::{
            PageRef, overflow,
            pager::{Pager, PagerConfig},
            slotted::SlottedPage,
            test_support::{TempDatabase, test_pager},
        },
    };

    #[test]
    fn insert_rejects_duplicate_physical_key() {
        let (_temp, mut pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        tree.insert(&mut batch, b"k", b"v1").unwrap();
        assert_eq!(
            tree.insert(&mut batch, b"k", b"v2").unwrap_err().code(),
            ErrorCode::Constraint
        );
    }

    #[test]
    fn inserts_grow_root_and_keep_all_values_visible() {
        let (_temp, mut pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        let old_root = tree.root();
        for i in 0..600u32 {
            tree.insert(&mut batch, &i.to_be_bytes(), &[i as u8; 40])
                .unwrap();
        }
        assert_ne!(tree.root(), old_root);
        batch.commit().unwrap();
        assert_eq!(
            tree.get(&mut pager, &599u32.to_be_bytes())
                .unwrap()
                .unwrap(),
            vec![599u32 as u8; 40]
        );
    }

    #[test]
    fn large_values_round_trip_through_overflow_pages() {
        let (_temp, mut pager) = test_pager(4096).unwrap();
        let value = vec![0x5a; 9000];
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        tree.insert(&mut batch, b"large", &value).unwrap();
        batch.commit().unwrap();

        assert_eq!(tree.get(&mut pager, b"large").unwrap(), Some(value));
    }

    #[test]
    fn inline_threshold_uses_compacted_free_space_at_the_exact_boundary() {
        assert!(matches!(
            insert_into_fragmented_leaf(4005).unwrap(),
            ValueStorage::Inline(value) if value.len() == 4005
        ));
        assert!(matches!(
            insert_into_fragmented_leaf(4006).unwrap(),
            ValueStorage::Overflow(_)
        ));
    }

    #[test]
    fn allocation_failure_restores_the_batch_for_a_later_insert() {
        let temp = TempDatabase::new("btree-mutation-rollback");
        let mut config = PagerConfig::test_default(4096);
        config.max_database_bytes = 3 * 4096;
        let mut pager = Pager::create(temp.path(), config).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();

        assert_eq!(
            tree.insert(&mut batch, b"too-large", &vec![0x7b; 9000])
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        tree.insert(&mut batch, b"ok", b"value").unwrap();
        batch.commit().unwrap();

        assert_eq!(pager.page_count(), 2);
        assert_eq!(
            tree.get(&mut pager, b"ok").unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(tree.get(&mut pager, b"too-large").unwrap(), None);
    }

    #[test]
    fn deterministic_orders_preserve_values_scans_and_exact_separators() {
        const KEY_COUNT: u32 = 80;
        for page_size in [4096, 8192, 16384] {
            let ascending = (0..KEY_COUNT).collect::<Vec<_>>();
            let descending = (0..KEY_COUNT).rev().collect::<Vec<_>>();
            let shuffled = (0..KEY_COUNT)
                .map(|index| (index * 37) % KEY_COUNT)
                .collect::<Vec<_>>();
            for (name, order) in [
                ("ascending", ascending),
                ("descending", descending),
                ("shuffled", shuffled),
            ] {
                verify_insert_order(page_size, name, &order).unwrap();
            }
        }
    }

    fn verify_insert_order(page_size: u32, name: &str, order: &[u32]) -> Result<()> {
        let (_temp, mut pager) = test_pager(page_size)?;
        let mut batch = pager.begin_write()?;
        let mut tree = BTree::create(&mut batch, TreeKind::Table)?;
        let mut inserted = Vec::new();
        for value in order {
            let key = test_key(*value);
            let before = batch.page_count()?;
            tree.insert(&mut batch, &key, &test_value(*value))?;
            inserted.push(*value);
            if batch.page_count()? > before {
                assert_all_visible(&mut batch, &tree, &inserted, name)?;
                assert_exact_separators(&mut batch, &tree)?;
            }
        }
        assert_all_visible(&mut batch, &tree, &inserted, name)?;
        assert_exact_separators(&mut batch, &tree)?;
        batch.commit()?;
        for value in order {
            assert_eq!(
                tree.get(&mut pager, &test_key(*value))?,
                Some(test_value(*value)),
                "{name} lookup failed at page size {page_size} for {value}"
            );
        }
        let mut scanned = Vec::new();
        let mut cursor = tree.scan(&mut pager, Bound::Unbounded, Bound::Unbounded)?;
        while let Some((key, value)) = cursor.next()? {
            scanned.push((key, value));
        }
        let mut expected = order
            .iter()
            .map(|value| (test_key(*value), test_value(*value)))
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(scanned, expected, "{name} committed scan at {page_size}");
        Ok(())
    }

    fn assert_all_visible(
        source: &mut impl PageSource,
        tree: &BTree,
        inserted: &[u32],
        name: &str,
    ) -> Result<()> {
        for value in inserted {
            assert_eq!(
                get_from_source(source, tree, &test_key(*value))?,
                Some(test_value(*value)),
                "{name} batch lookup failed for {value}"
            );
        }
        let mut expected = inserted
            .iter()
            .map(|value| test_key(*value))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(scan_keys(source, tree)?, expected, "{name} scan order");
        Ok(())
    }

    fn get_from_source(
        source: &mut impl PageSource,
        tree: &BTree,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let mut current = tree.root;
        loop {
            match load_node(source, tree.kind, current)? {
                Node::Internal(node) => {
                    let child_index = node.cells.partition_point(|cell| cell.key() <= key);
                    current = if child_index == 0 {
                        node.leftmost
                    } else {
                        node.cells[child_index - 1].right()
                    };
                }
                Node::Leaf(node) => {
                    let index = lower_bound(&node.cells, key);
                    let Some(cell) = node.cells.get(index).filter(|cell| cell.key() == key) else {
                        return Ok(None);
                    };
                    return match cell.storage() {
                        ValueStorage::Inline(value) => Ok(Some(value.clone())),
                        ValueStorage::Overflow(first) => {
                            overflow::read_value(source, *first, cell.total_value_len()).map(Some)
                        }
                    };
                }
            }
        }
    }

    fn scan_keys(source: &mut impl PageSource, tree: &BTree) -> Result<Vec<Vec<u8>>> {
        let mut current = tree.root;
        while let Node::Internal(node) = load_node(source, tree.kind, current)? {
            current = node.leftmost;
        }
        let mut keys = Vec::new();
        let mut visited = HashSet::new();
        loop {
            assert!(visited.insert(current), "leaf sibling cycle");
            let Node::Leaf(leaf) = load_node(source, tree.kind, current)? else {
                panic!("leaf chain reached an internal page");
            };
            keys.extend(leaf.cells.iter().map(|cell| cell.key().to_vec()));
            let Some(next) = leaf.right_sibling else {
                return Ok(keys);
            };
            current = next;
        }
    }

    fn assert_exact_separators(source: &mut impl PageSource, tree: &BTree) -> Result<()> {
        let mut visited = HashSet::new();
        subtree_minimum(source, tree.kind, tree.root, &mut visited).map(|_| ())
    }

    fn subtree_minimum(
        source: &mut impl PageSource,
        kind: TreeKind,
        reference: PageRef,
        visited: &mut HashSet<PageRef>,
    ) -> Result<Vec<u8>> {
        assert!(
            visited.insert(reference),
            "tree child cycle or duplicate child"
        );
        match load_node(source, kind, reference)? {
            Node::Leaf(leaf) => Ok(leaf
                .cells
                .first()
                .expect("nonempty inserted leaf")
                .key()
                .to_vec()),
            Node::Internal(node) => {
                let minimum = subtree_minimum(source, kind, node.leftmost, visited)?;
                for cell in &node.cells {
                    let right_minimum = subtree_minimum(source, kind, cell.right(), visited)?;
                    assert_eq!(
                        cell.key(),
                        right_minimum,
                        "separator must equal right minimum"
                    );
                }
                Ok(minimum)
            }
        }
    }

    fn test_key(value: u32) -> Vec<u8> {
        let mut key = vec![0x61; 1024];
        key[..4].copy_from_slice(&value.to_be_bytes());
        key
    }

    fn test_value(value: u32) -> Vec<u8> {
        vec![value as u8; 40]
    }

    fn insert_into_fragmented_leaf(value_len: usize) -> Result<ValueStorage> {
        let (_temp, mut pager) = test_pager(4096)?;
        let mut batch = pager.begin_write()?;
        let mut tree = BTree::create(&mut batch, TreeKind::Table)?;
        let root = tree.root();
        let existing = super::super::LeafCell::inline(b"a".to_vec(), b"x".to_vec())?.encode()?;
        {
            let frame = batch.modify(root)?;
            SlottedPage::open(frame, super::super::LEAF_METADATA_END)?.insert(0, &existing)?;
            let old_offset = usize::from(u16::from_le_bytes(
                frame.bytes()[56..58]
                    .try_into()
                    .expect("slot offset is two bytes"),
            ));
            let fragmented_offset = old_offset - 64;
            frame
                .bytes_mut()
                .copy_within(old_offset..old_offset + existing.len(), fragmented_offset);
            frame.bytes_mut()[56..58].copy_from_slice(
                &u16::try_from(fragmented_offset)
                    .expect("fragmented offset fits u16")
                    .to_le_bytes(),
            );
            frame.set_layout(1, 60, u16::try_from(fragmented_offset).unwrap())?;
            let page = SlottedPage::open(frame, super::super::LEAF_METADATA_END)?;
            assert_eq!(page.compacted_free()?, 4022);
            assert_eq!(page.contiguous_free()?, 3958);
        }

        tree.insert(&mut batch, b"b", &vec![0x5a; value_len])?;
        let Node::Leaf(leaf) = load_node(&mut batch, TreeKind::Table, root)? else {
            panic!("threshold insert unexpectedly split the root");
        };
        Ok(leaf.cells[1].storage().clone())
    }
}
