use std::collections::HashSet;

use super::{
    INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, LeafCell, encode_internal_leftmost,
    encode_leaf_right_sibling,
    search::{BTree, InternalNode, LeafNode, Node, load_node, lower_bound},
    validate_key,
};
use crate::storage::{
    PageRef, overflow,
    page::PageFrame,
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
        batch: &mut WriteBatch,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        validate_key(key)?;
        let old_root = self.root;
        let checkpoint = batch.begin_mutation_scope()?;
        let result = self
            .insert_inner(batch, key, value)
            .and_then(|()| batch.refresh_tree(self.kind, old_root, self.root));
        match result {
            Ok(()) => batch.finish_mutation_scope(checkpoint),
            Err(error) => {
                self.root = old_root;
                batch.rollback_mutation_scope(checkpoint)?;
                Err(error)
            }
        }
    }

    fn insert_inner(&mut self, batch: &mut WriteBatch, key: &[u8], value: &[u8]) -> Result<()> {
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

    pub(in crate::storage) fn delete(
        &mut self,
        batch: &mut WriteBatch,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        validate_key(key)?;
        let old_root = self.root;
        let checkpoint = batch.begin_mutation_scope()?;
        let result = self.delete_inner(batch, key).and_then(|removed| {
            if removed.is_some() {
                batch.refresh_tree(self.kind, old_root, self.root)?;
            }
            Ok(removed)
        });
        match result {
            Ok(removed) => {
                batch.finish_mutation_scope(checkpoint)?;
                Ok(removed)
            }
            Err(error) => {
                self.root = old_root;
                batch.rollback_mutation_scope(checkpoint)?;
                Err(error)
            }
        }
    }

    fn delete_inner(&mut self, batch: &mut WriteBatch, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (leaf_ref, mut leaf, path) = descend_with_path(batch, self.kind, self.root, key)?;
        let delete_at = lower_bound(&leaf.cells, key);
        let Some(cell) = leaf.cells.get(delete_at).filter(|cell| cell.key() == key) else {
            return Ok(None);
        };
        let removed = read_cell_value(batch, cell)?;
        if let super::ValueStorage::Overflow(first) = cell.storage() {
            for reference in overflow::collect_chain(batch, *first, cell.total_value_len())? {
                batch.release_btree_later(reference)?;
            }
        }
        leaf.cells.remove(delete_at);
        repair_after_delete(
            batch,
            self.kind,
            &mut self.root,
            leaf_ref,
            Node::Leaf(leaf),
            path,
        )?;
        Ok(Some(removed))
    }
}

fn read_cell_value(batch: &mut WriteBatch, cell: &LeafCell) -> Result<Vec<u8>> {
    match cell.storage() {
        super::ValueStorage::Inline(value) => Ok(value.clone()),
        super::ValueStorage::Overflow(first) => {
            overflow::read_value(batch, *first, cell.total_value_len())
        }
    }
}

fn repair_after_delete(
    batch: &mut WriteBatch,
    kind: super::TreeKind,
    root: &mut PageRef,
    mut current_ref: PageRef,
    mut current: Node,
    mut path: SearchPath,
) -> Result<()> {
    loop {
        if current_ref == *root {
            match current {
                Node::Leaf(leaf) => {
                    rewrite_leaf(batch, current_ref, leaf.right_sibling, &leaf.cells)?
                }
                Node::Internal(internal) if internal.cells.is_empty() => {
                    batch.replace_tree_root(kind, current_ref, internal.leftmost)?;
                    *root = internal.leftmost;
                    batch.release_btree_later(current_ref)?;
                }
                Node::Internal(internal) => {
                    rewrite_internal(batch, current_ref, internal.leftmost, &internal.cells)?;
                }
            }
            return Ok(());
        }

        let page_len = batch.read(current_ref)?.bytes().len();
        if !node_is_underfull(page_len, &current)? {
            rewrite_node(batch, current_ref, &current)?;
            let minimum = node_minimum(batch, kind, &current)?;
            propagate_minimum(batch, kind, current_ref, minimum, path)?;
            return Ok(());
        }

        let (parent_ref, child_index) = path
            .pop()
            .ok_or_else(|| corruption("B+Tree deletion path ended before the root"))?;
        let mut parent = match load_node(batch, kind, parent_ref)? {
            Node::Internal(parent) => parent,
            Node::Leaf(_) => return Err(corruption("B+Tree path parent is a leaf page")),
        };
        if child_index > parent.cells.len() || parent_child(&parent, child_index)? != current_ref {
            return Err(corruption("B+Tree deletion path does not match its parent"));
        }
        if child_index > 0 && !matches!(&current, Node::Leaf(leaf) if leaf.cells.is_empty()) {
            let minimum = node_minimum(batch, kind, &current)?;
            parent.cells[child_index - 1] = InternalCell::new(minimum, current_ref)?;
        }

        let left = if child_index > 0 {
            let reference = parent_child(&parent, child_index - 1)?;
            Some((reference, load_node(batch, kind, reference)?))
        } else {
            None
        };
        let right = if child_index < parent.cells.len() {
            let reference = parent_child(&parent, child_index + 1)?;
            Some((reference, load_node(batch, kind, reference)?))
        } else {
            None
        };

        let left_borrow = if let Some((_, left_node)) = &left {
            borrow_candidate(
                page_len,
                left_node,
                &current,
                parent.cells[child_index - 1].key(),
                BorrowSide::Left,
            )?
        } else {
            None
        };
        let right_borrow = if let Some((_, right_node)) = &right {
            borrow_candidate(
                page_len,
                right_node,
                &current,
                parent.cells[child_index].key(),
                BorrowSide::Right,
            )?
        } else {
            None
        };

        let use_left = match (&left_borrow, &right_borrow) {
            (Some(left), Some(right)) => left.moved <= right.moved,
            (Some(_), None) => true,
            _ => false,
        };
        if use_left {
            let candidate = left_borrow.expect("left borrow exists");
            let (left_ref, _) = left.expect("left sibling exists");
            rewrite_node(batch, left_ref, &candidate.donor)?;
            rewrite_node(batch, current_ref, &candidate.receiver)?;
            parent.cells[child_index - 1] = InternalCell::new(candidate.separator, current_ref)?;
            rewrite_internal(batch, parent_ref, parent.leftmost, &parent.cells)?;
            let parent_minimum = subtree_minimum(batch, kind, parent_ref)?;
            propagate_minimum(batch, kind, parent_ref, parent_minimum, path)?;
            return Ok(());
        }
        if let Some(candidate) = right_borrow {
            let (right_ref, _) = right.expect("right sibling exists");
            if child_index > 0 {
                let minimum = node_minimum(batch, kind, &candidate.receiver)?;
                parent.cells[child_index - 1] = InternalCell::new(minimum, current_ref)?;
            }
            rewrite_node(batch, right_ref, &candidate.donor)?;
            rewrite_node(batch, current_ref, &candidate.receiver)?;
            parent.cells[child_index] = InternalCell::new(candidate.separator, right_ref)?;
            rewrite_internal(batch, parent_ref, parent.leftmost, &parent.cells)?;
            let parent_minimum = subtree_minimum(batch, kind, parent_ref)?;
            propagate_minimum(batch, kind, parent_ref, parent_minimum, path)?;
            return Ok(());
        }

        if let Some((left_ref, left_node)) = &left {
            let separator = parent.cells[child_index - 1].key().to_vec();
            let merged = merge_nodes(left_node, &current, separator)?;
            if node_fits(page_len, &merged)? {
                rewrite_node(batch, *left_ref, &merged)?;
                batch.release_btree_later(current_ref)?;
                parent.cells.remove(child_index - 1);
                current_ref = parent_ref;
                current = Node::Internal(parent);
                continue;
            }
        }

        let Some((right_ref, right_node)) = right else {
            return Err(corruption("underfull B+Tree page has no mergeable sibling"));
        };
        let separator = parent.cells[child_index].key().to_vec();
        let merged = merge_nodes(&current, &right_node, separator)?;
        if !node_fits(page_len, &merged)? {
            return Err(corruption(
                "underfull B+Tree siblings do not fit after compaction",
            ));
        }
        rewrite_node(batch, current_ref, &merged)?;
        batch.release_btree_later(right_ref)?;
        parent.cells.remove(child_index);
        current_ref = parent_ref;
        current = Node::Internal(parent);
    }
}

#[derive(Clone, Copy)]
enum BorrowSide {
    Left,
    Right,
}

struct BorrowCandidate {
    moved: usize,
    donor: Node,
    receiver: Node,
    separator: Vec<u8>,
}

fn borrow_candidate(
    page_len: usize,
    donor: &Node,
    receiver: &Node,
    parent_separator: &[u8],
    side: BorrowSide,
) -> Result<Option<BorrowCandidate>> {
    let mut donor = clone_node(donor);
    let mut receiver = clone_node(receiver);
    let mut separator = parent_separator.to_vec();
    let maximum = node_cell_count(&donor).saturating_sub(1);
    for moved in 1..=maximum {
        rotate_one(&mut donor, &mut receiver, &mut separator, side)?;
        if !node_is_underfull(page_len, &receiver)? && !node_is_underfull(page_len, &donor)? {
            return Ok(Some(BorrowCandidate {
                moved,
                donor,
                receiver,
                separator,
            }));
        }
    }
    Ok(None)
}

fn rotate_one(
    donor: &mut Node,
    receiver: &mut Node,
    separator: &mut Vec<u8>,
    side: BorrowSide,
) -> Result<()> {
    match (donor, receiver, side) {
        (Node::Leaf(donor), Node::Leaf(receiver), BorrowSide::Left) => {
            let cell = donor
                .cells
                .pop()
                .ok_or_else(|| corruption("left leaf donor is empty"))?;
            receiver.cells.insert(0, cell);
            *separator = receiver.cells[0].key().to_vec();
        }
        (Node::Leaf(donor), Node::Leaf(receiver), BorrowSide::Right) => {
            if donor.cells.is_empty() {
                return Err(corruption("right leaf donor is empty"));
            }
            receiver.cells.push(donor.cells.remove(0));
            *separator = donor
                .cells
                .first()
                .ok_or_else(|| corruption("right leaf borrow emptied its donor"))?
                .key()
                .to_vec();
        }
        (Node::Internal(donor), Node::Internal(receiver), BorrowSide::Left) => {
            let cell = donor
                .cells
                .pop()
                .ok_or_else(|| corruption("left internal donor is empty"))?;
            let old_receiver_leftmost = receiver.leftmost;
            receiver.leftmost = cell.right();
            receiver.cells.insert(
                0,
                InternalCell::new(separator.clone(), old_receiver_leftmost)?,
            );
            *separator = cell.key().to_vec();
        }
        (Node::Internal(donor), Node::Internal(receiver), BorrowSide::Right) => {
            if donor.cells.is_empty() {
                return Err(corruption("right internal donor is empty"));
            }
            let cell = donor.cells.remove(0);
            receiver
                .cells
                .push(InternalCell::new(separator.clone(), donor.leftmost)?);
            donor.leftmost = cell.right();
            *separator = cell.key().to_vec();
        }
        _ => return Err(corruption("B+Tree siblings have different node types")),
    }
    Ok(())
}

fn merge_nodes(left: &Node, right: &Node, separator: Vec<u8>) -> Result<Node> {
    match (left, right) {
        (Node::Leaf(left), Node::Leaf(right)) => {
            let mut cells = left.cells.clone();
            cells.extend(right.cells.iter().cloned());
            Ok(Node::Leaf(LeafNode {
                cells,
                right_sibling: right.right_sibling,
            }))
        }
        (Node::Internal(left), Node::Internal(right)) => {
            let mut cells = left.cells.clone();
            cells.push(InternalCell::new(separator, right.leftmost)?);
            cells.extend(right.cells.iter().cloned());
            Ok(Node::Internal(InternalNode {
                leftmost: left.leftmost,
                cells,
            }))
        }
        _ => Err(corruption("B+Tree siblings have different node types")),
    }
}

fn clone_node(node: &Node) -> Node {
    match node {
        Node::Leaf(leaf) => Node::Leaf(LeafNode {
            cells: leaf.cells.clone(),
            right_sibling: leaf.right_sibling,
        }),
        Node::Internal(internal) => Node::Internal(InternalNode {
            leftmost: internal.leftmost,
            cells: internal.cells.clone(),
        }),
    }
}

fn node_cell_count(node: &Node) -> usize {
    match node {
        Node::Leaf(leaf) => leaf.cells.len(),
        Node::Internal(internal) => internal.cells.len(),
    }
}

fn node_is_underfull(page_len: usize, node: &Node) -> Result<bool> {
    let (metadata_end, encoded) = encoded_node_cells(node)?;
    let usable = page_len
        .checked_sub(metadata_end)
        .ok_or_else(|| corruption("B+Tree metadata exceeds page length"))?;
    let used = encoded_bytes(&encoded)?;
    Ok(used.checked_mul(4).is_none_or(|used| used < usable))
}

fn node_fits(page_len: usize, node: &Node) -> Result<bool> {
    let (metadata_end, encoded) = encoded_node_cells(node)?;
    encoded_cells_fit(page_len, metadata_end, &encoded)
}

fn encoded_node_cells(node: &Node) -> Result<(usize, Vec<Vec<u8>>)> {
    match node {
        Node::Leaf(leaf) => Ok((LEAF_METADATA_END, encode_leaf_cells(&leaf.cells)?)),
        Node::Internal(internal) => Ok((
            INTERNAL_METADATA_END,
            encode_internal_cells(&internal.cells)?,
        )),
    }
}

fn rewrite_node(batch: &mut WriteBatch, reference: PageRef, node: &Node) -> Result<()> {
    match node {
        Node::Leaf(leaf) => rewrite_leaf(batch, reference, leaf.right_sibling, &leaf.cells),
        Node::Internal(internal) => {
            rewrite_internal(batch, reference, internal.leftmost, &internal.cells)
        }
    }
}

fn parent_child(parent: &InternalNode, child_index: usize) -> Result<PageRef> {
    if child_index == 0 {
        Ok(parent.leftmost)
    } else {
        parent
            .cells
            .get(child_index - 1)
            .map(InternalCell::right)
            .ok_or_else(|| corruption("B+Tree child index is outside its parent"))
    }
}

fn node_minimum(batch: &mut WriteBatch, kind: super::TreeKind, node: &Node) -> Result<Vec<u8>> {
    match node {
        Node::Leaf(leaf) => leaf
            .cells
            .first()
            .map(|cell| cell.key().to_vec())
            .ok_or_else(|| corruption("non-root B+Tree leaf is empty")),
        Node::Internal(internal) => subtree_minimum(batch, kind, internal.leftmost),
    }
}

fn subtree_minimum(
    source: &mut impl PageSource,
    kind: super::TreeKind,
    mut reference: PageRef,
) -> Result<Vec<u8>> {
    let max_hops = source.page_count()?;
    let mut visited = HashSet::new();
    loop {
        if u64::try_from(visited.len()).map_or(true, |count| count >= max_hops)
            || !visited.insert(reference)
        {
            return Err(corruption("B+Tree minimum descent has a cycle"));
        }
        match load_node(source, kind, reference)? {
            Node::Leaf(leaf) => {
                return leaf
                    .cells
                    .first()
                    .map(|cell| cell.key().to_vec())
                    .ok_or_else(|| corruption("non-root B+Tree leaf is empty"));
            }
            Node::Internal(internal) => reference = internal.leftmost,
        }
    }
}

fn propagate_minimum(
    batch: &mut WriteBatch,
    kind: super::TreeKind,
    mut child_ref: PageRef,
    minimum: Vec<u8>,
    mut path: SearchPath,
) -> Result<()> {
    while let Some((parent_ref, child_index)) = path.pop() {
        let mut parent = match load_node(batch, kind, parent_ref)? {
            Node::Internal(parent) => parent,
            Node::Leaf(_) => return Err(corruption("B+Tree path parent is a leaf page")),
        };
        if parent_child(&parent, child_index)? != child_ref {
            return Err(corruption("B+Tree deletion path does not match its parent"));
        }
        if child_index == 0 {
            child_ref = parent_ref;
            continue;
        }
        parent.cells[child_index - 1] = InternalCell::new(minimum, child_ref)?;
        rewrite_internal(batch, parent_ref, parent.leftmost, &parent.cells)?;
        return Ok(());
    }
    Ok(())
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
    batch: &mut WriteBatch,
    reference: PageRef,
    metadata_end: usize,
    encoded_len: usize,
) -> Result<bool> {
    let handle = batch.read(reference)?;
    let mut frame = PageFrame::decode(
        handle.bytes().to_vec(),
        reference.page_id(),
        None,
        Some(reference.generation()),
    )?;
    let free = SlottedPage::open(&mut frame, metadata_end)?.compacted_free()?;
    Ok(encoded_len
        .checked_add(SLOT_LEN)
        .is_some_and(|required| required <= free))
}

fn write_or_split_leaf(
    batch: &mut WriteBatch,
    kind: super::TreeKind,
    left_ref: PageRef,
    leaf: LeafNode,
) -> Result<Option<Split>> {
    let encoded = encode_leaf_cells(&leaf.cells)?;
    let page_len = batch.read(left_ref)?.bytes().len();
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
    batch: &mut WriteBatch,
    kind: super::TreeKind,
    left_ref: PageRef,
    node: InternalNode,
) -> Result<Option<Split>> {
    let encoded = encode_internal_cells(&node.cells)?;
    let page_len = batch.read(left_ref)?.bytes().len();
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
    batch: &mut WriteBatch,
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
    batch: &mut WriteBatch,
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
        InternalCell, LeafCell, Node, PageSource, load_node, lower_bound, rewrite_internal,
        rewrite_leaf,
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
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        tree.insert(&mut batch, b"k", b"v1").unwrap();
        assert_eq!(
            tree.insert(&mut batch, b"k", b"v2").unwrap_err().code(),
            ErrorCode::Constraint
        );
    }

    #[test]
    fn held_clean_handle_allows_insert_without_a_refcell_panic() {
        let (_temp, pager) = test_pager(4096).expect("test pager");
        let mut tree = {
            let mut batch = pager.begin_write().expect("begin seed write");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            tree.insert(&mut batch, b"a", b"one")
                .expect("insert seed row");
            batch.commit().expect("commit seed row");
            tree
        };
        let handle = pager.read(tree.root()).expect("hold clean root handle");
        let _held_frame = handle.as_ref();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut batch = pager.begin_write()?;
            tree.insert(&mut batch, b"b", b"two")?;
            batch.rollback()
        }));

        result
            .expect("insert must not panic while a clean frame is immutably borrowed")
            .expect("insert must retain normal supported behavior");
    }

    #[test]
    fn delete_returns_value_and_missing_key_is_noop() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut tree = {
            let mut batch = pager.begin_write().unwrap();
            let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
            for i in 0..300u32 {
                tree.insert(&mut batch, &i.to_be_bytes(), &[i as u8; 40])
                    .unwrap();
            }
            batch.commit().unwrap();
            tree
        };
        let page_count = pager.page_count();
        let mut batch = pager.begin_write().unwrap();
        assert_eq!(tree.delete(&mut batch, b"missing").unwrap(), None);
        assert_eq!(batch.page_count().unwrap(), page_count);
        assert_eq!(
            tree.delete(&mut batch, &150u32.to_be_bytes()).unwrap(),
            Some(vec![150u32 as u8; 40])
        );
        batch.commit().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        assert_eq!(
            tree.get(&mut snapshot, &150u32.to_be_bytes()).unwrap(),
            None
        );
    }

    #[test]
    fn missing_delete_preserves_an_unregistered_tree_and_existing_batch_state() {
        let (_temp, pager) = test_pager(4096).expect("create pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin tree batch");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            batch.commit().expect("commit unregistered tree");
            tree
        };
        let root = tree.root();
        let mut tree = tree;
        let mut batch = pager
            .begin_write()
            .expect("begin batch with existing state");
        let existing = batch
            .allocate(TreeKind::Index.leaf_page_type())
            .expect("allocate unrelated page");

        assert_eq!(
            tree.delete(&mut batch, b"missing").expect("missing delete"),
            None
        );
        assert_eq!(
            tree.root(),
            root,
            "missing delete must preserve the tree root"
        );
        assert_eq!(batch.page_count().expect("batch page count"), 3);
        assert_eq!(
            batch
                .set_catalog_root(root)
                .expect_err("missing delete must not register an untracked root")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            batch
                .read(existing)
                .expect("existing unrelated batch page")
                .header()
                .page_type(),
            TreeKind::Index.leaf_page_type()
        );
        batch.rollback().expect("rollback existing state");
    }

    #[test]
    fn equal_byte_balance_split_ties_choose_the_lower_leaf_and_internal_indexes() {
        let leaf_cells = vec![vec![0x11; 10]; 5];
        assert_eq!(
            super::balanced_leaf_boundary(4096, &leaf_cells).expect("balance leaf split"),
            2,
            "equal two-versus-three and three-versus-two byte balances choose the lower split"
        );

        let internal_cells = vec![vec![0x22; 10]; 6];
        assert_eq!(
            super::balanced_internal_separator(4096, &internal_cells)
                .expect("balance internal split"),
            2,
            "equal left and right internal byte balances choose the lower promoted cell"
        );
    }

    #[test]
    fn delete_returns_overflow_value_and_releases_the_whole_chain() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager) = test_pager(page_size).unwrap();
            let value = vec![0x5a; usize::try_from(page_size).unwrap() * 3];
            let (mut tree, overflow_pages) = {
                let mut batch = pager.begin_write().unwrap();
                let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
                tree.insert(&mut batch, b"large", &value).unwrap();
                let Node::Leaf(leaf) = load_node(&mut batch, TreeKind::Table, tree.root()).unwrap()
                else {
                    panic!("single large value must remain in a leaf root");
                };
                let ValueStorage::Overflow(first) = leaf.cells[0].storage() else {
                    panic!("large value must use overflow pages");
                };
                let pages =
                    overflow::collect_chain(&mut batch, *first, leaf.cells[0].total_value_len())
                        .unwrap();
                batch.commit().unwrap();
                (tree, pages)
            };

            {
                let mut batch = pager.begin_write().unwrap();
                assert_eq!(tree.delete(&mut batch, b"large").unwrap(), Some(value));
                batch.commit().unwrap();
            }
            let replacements = {
                let mut batch = pager.begin_write().unwrap();
                let pages = (0..overflow_pages.len())
                    .map(|_| batch.allocate(TreeKind::Table.leaf_page_type()).unwrap())
                    .collect::<Vec<_>>();
                for page in &pages {
                    SlottedPage::open(
                        batch.modify(*page).unwrap(),
                        super::super::LEAF_METADATA_END,
                    )
                    .unwrap();
                }
                batch.commit().unwrap();
                pages
            };
            assert_eq!(
                replacements
                    .iter()
                    .map(|page| page.page_id())
                    .collect::<HashSet<_>>(),
                overflow_pages
                    .iter()
                    .map(|page| page.page_id())
                    .collect::<HashSet<_>>()
            );
            for stale in overflow_pages {
                assert_eq!(pager.read(stale).unwrap_err().code(), ErrorCode::Corruption);
            }
        }
    }

    #[test]
    fn deletion_error_restores_partially_staged_overflow_releases() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager) = test_pager(page_size).unwrap();
            let value = vec![0x6b; usize::try_from(page_size).unwrap() * 3];
            let (mut tree, first, chain) = {
                let mut batch = pager.begin_write().unwrap();
                let target = batch.allocate(TreeKind::Table.leaf_page_type()).unwrap();
                let first = overflow::write_value(&mut batch, &value).unwrap().unwrap();
                let chain =
                    overflow::collect_chain(&mut batch, first, u64::try_from(value.len()).unwrap())
                        .unwrap();
                let wrong_sibling = overflow::write_value(&mut batch, b"wrong type")
                    .unwrap()
                    .unwrap();
                rewrite_leaf(
                    &mut batch,
                    target,
                    Some(wrong_sibling),
                    &[LeafCell::overflow(
                        b"a".to_vec(),
                        u64::try_from(value.len()).unwrap(),
                        first,
                    )
                    .unwrap()],
                )
                .unwrap();
                let root = batch
                    .allocate(TreeKind::Table.internal_page_type())
                    .unwrap();
                rewrite_internal(
                    &mut batch,
                    root,
                    target,
                    &[InternalCell::new(b"z".to_vec(), wrong_sibling).unwrap()],
                )
                .unwrap();
                batch.commit().unwrap();
                (
                    BTree {
                        kind: TreeKind::Table,
                        root,
                    },
                    first,
                    chain,
                )
            };
            let old_root = tree.root();
            {
                let mut batch = pager.begin_write().unwrap();
                assert_eq!(
                    tree.delete(&mut batch, b"a").unwrap_err().code(),
                    ErrorCode::Corruption
                );
                assert_eq!(tree.root(), old_root);
                assert_eq!(
                    overflow::read_value(&mut batch, first, u64::try_from(value.len()).unwrap(),)
                        .unwrap(),
                    value
                );
                assert_eq!(
                    get_from_source(&mut batch, &tree, b"a").unwrap(),
                    Some(value.clone())
                );
                batch.commit().unwrap();
            }
            let mut snapshot = pager.begin_read().unwrap();
            assert_eq!(tree.get(&mut snapshot, b"a").unwrap(), Some(value));
            let mut batch = pager.begin_write().unwrap();
            let appended = batch.allocate(TreeKind::Table.leaf_page_type()).unwrap();
            assert!(
                !chain
                    .iter()
                    .any(|page| page.page_id() == appended.page_id())
            );
            batch.rollback().unwrap();
        }
    }

    #[test]
    fn deleting_to_empty_shrinks_to_one_leaf_root() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        for i in 0..600u32 {
            tree.insert(&mut batch, &i.to_be_bytes(), &[i as u8; 40])
                .unwrap();
        }
        for i in 0..600u32 {
            tree.delete(&mut batch, &i.to_be_bytes()).unwrap();
        }
        let Node::Leaf(root) = load_node(&mut batch, TreeKind::Table, tree.root()).unwrap() else {
            panic!("empty tree root must be a leaf");
        };
        assert!(root.cells.is_empty());
        assert_eq!(root.right_sibling, None);
    }

    #[test]
    fn leaf_borrows_from_left_then_right_at_every_page_size() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager, mut tree, left_ref, right_ref) =
                committed_two_leaf_tree(page_size, 5, 3).unwrap();
            let mut batch = pager.begin_write().unwrap();
            assert_eq!(
                tree.delete(&mut batch, &small_key(5)).unwrap(),
                Some(sized_value(5, leaf_test_value_len(page_size)))
            );
            assert_eq!(leaf_keys(&mut batch, left_ref), vec![0, 1, 2, 3]);
            assert_eq!(leaf_keys(&mut batch, right_ref), vec![4, 6, 7]);
            assert_eq!(
                scan_small_keys(&mut batch, &tree).unwrap(),
                vec![0, 1, 2, 3, 4, 6, 7]
            );
            assert_exact_separators(&mut batch, &tree).unwrap();
            assert_eq!(leaf_right_sibling(&mut batch, left_ref), Some(right_ref));

            let (_temp, pager, mut tree, left_ref, right_ref) =
                committed_two_leaf_tree(page_size, 3, 5).unwrap();
            let mut batch = pager.begin_write().unwrap();
            assert_eq!(
                tree.delete(&mut batch, &small_key(2)).unwrap(),
                Some(sized_value(2, leaf_test_value_len(page_size)))
            );
            assert_eq!(leaf_keys(&mut batch, left_ref), vec![0, 1, 3]);
            assert_eq!(leaf_keys(&mut batch, right_ref), vec![4, 5, 6, 7]);
            assert_eq!(
                scan_small_keys(&mut batch, &tree).unwrap(),
                vec![0, 1, 3, 4, 5, 6, 7]
            );
            assert_exact_separators(&mut batch, &tree).unwrap();
            assert_eq!(leaf_right_sibling(&mut batch, left_ref), Some(right_ref));
        }
    }

    #[test]
    fn right_borrow_into_an_empty_nonleftmost_leaf_refreshes_its_parent_separator() {
        let (_temp, pager, mut tree, left, middle, right) =
            committed_three_leaf_tree(4096, 3, 1, 7).expect("create three-leaf tree");
        let mut batch = pager.begin_write().expect("begin delete batch");

        assert_eq!(
            tree.delete(&mut batch, &small_key(3))
                .expect("delete only middle key"),
            Some(sized_value(3, leaf_test_value_len(4096)))
        );
        assert_eq!(leaf_keys(&mut batch, left), vec![0, 1, 2]);
        assert_eq!(leaf_keys(&mut batch, middle), vec![4, 5, 6]);
        assert_eq!(leaf_keys(&mut batch, right), vec![7, 8, 9, 10]);
        assert_exact_separators(&mut batch, &tree).expect("parent separators after right borrow");
    }

    #[test]
    fn equal_cost_borrow_and_merge_prefer_the_left_sibling() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager, mut tree, left, middle, right) =
                committed_three_leaf_tree(page_size, 5, 3, 5).unwrap();
            let mut batch = pager.begin_write().unwrap();
            tree.delete(&mut batch, &small_key(5)).unwrap();
            assert_eq!(leaf_keys(&mut batch, left), vec![0, 1, 2, 3]);
            assert_eq!(leaf_keys(&mut batch, middle), vec![4, 6, 7]);
            assert_eq!(leaf_keys(&mut batch, right), vec![8, 9, 10, 11, 12]);
            assert_exact_separators(&mut batch, &tree).unwrap();

            let (_temp, pager, mut tree, left, _middle, right) =
                committed_three_leaf_tree(page_size, 3, 3, 3).unwrap();
            let mut batch = pager.begin_write().unwrap();
            tree.delete(&mut batch, &small_key(3)).unwrap();
            assert_eq!(leaf_keys(&mut batch, left), vec![0, 1, 2, 4, 5]);
            assert_eq!(leaf_keys(&mut batch, right), vec![6, 7, 8]);
            assert_eq!(leaf_right_sibling(&mut batch, left), Some(right));
            assert_exact_separators(&mut batch, &tree).unwrap();
        }
    }

    #[test]
    fn leaf_merge_shrinks_root_and_delays_reuse_at_every_page_size() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager, mut tree, left_ref, right_ref) =
                committed_two_leaf_tree(page_size, 3, 3).unwrap();
            let old_root = tree.root();
            let appended = {
                let mut batch = pager.begin_write().unwrap();
                assert_eq!(
                    tree.delete(&mut batch, &small_key(2)).unwrap(),
                    Some(sized_value(2, leaf_test_value_len(page_size)))
                );
                assert_eq!(tree.root(), left_ref);
                assert_eq!(leaf_right_sibling(&mut batch, left_ref), None);
                assert_eq!(
                    scan_small_keys(&mut batch, &tree).unwrap(),
                    vec![0, 1, 3, 4, 5]
                );
                let appended = batch.allocate(TreeKind::Table.leaf_page_type()).unwrap();
                assert_ne!(appended.page_id(), right_ref.page_id());
                assert_ne!(appended.page_id(), old_root.page_id());
                SlottedPage::open(
                    batch.modify(appended).unwrap(),
                    super::super::LEAF_METADATA_END,
                )
                .unwrap();
                batch.commit().unwrap();
                appended
            };

            let replacements = {
                let mut batch = pager.begin_write().unwrap();
                let replacements = (0..2)
                    .map(|_| batch.allocate(TreeKind::Table.leaf_page_type()).unwrap())
                    .collect::<Vec<_>>();
                for replacement in &replacements {
                    SlottedPage::open(
                        batch.modify(*replacement).unwrap(),
                        super::super::LEAF_METADATA_END,
                    )
                    .unwrap();
                }
                batch.commit().unwrap();
                replacements
            };
            let replacement_ids = replacements
                .iter()
                .map(|page| page.page_id())
                .collect::<HashSet<_>>();
            assert_eq!(
                replacement_ids,
                HashSet::from([right_ref.page_id(), old_root.page_id()])
            );
            assert!(replacements.iter().all(|page| page.generation() == 2));
            assert_eq!(
                pager.read(right_ref).unwrap_err().code(),
                ErrorCode::Corruption
            );
            assert_eq!(
                pager.read(old_root).unwrap_err().code(),
                ErrorCode::Corruption
            );
            pager.read(appended).unwrap();
        }
    }

    #[test]
    fn deleting_a_catalog_root_can_shrink_to_its_left_leaf() {
        let (_temp, pager, mut tree, left_ref, _right_ref) =
            committed_two_leaf_tree(4096, 3, 3).expect("create two-leaf tree");
        let old_root = tree.root();
        {
            let mut batch = pager.begin_write().expect("begin catalog publish batch");
            batch
                .track_tree(TreeKind::Table, old_root)
                .expect("register catalog tree");
            batch
                .set_catalog_root(old_root)
                .expect("publish catalog root");
            batch.commit().expect("commit catalog root");
        }

        {
            let mut batch = pager
                .begin_write()
                .expect("begin catalog root shrink batch");
            assert_eq!(
                tree.delete(&mut batch, &small_key(2))
                    .expect("delete from the catalog tree"),
                Some(sized_value(2, leaf_test_value_len(4096)))
            );
            assert_eq!(tree.root(), left_ref);
            batch
                .set_catalog_root(tree.root())
                .expect("publish the shrunken catalog root");
            batch.commit().expect("commit catalog root shrink");
        }

        let mut snapshot = pager.begin_read().expect("begin catalog snapshot");
        assert_eq!(
            snapshot
                .catalog_tree(TreeKind::Table)
                .expect("read catalog root")
                .expect("catalog root is present")
                .root(),
            left_ref
        );
    }

    #[test]
    fn internal_nodes_borrow_from_both_sides_at_every_page_size() {
        for page_size in [4096, 8192, 16384] {
            let minimum_cells = minimum_large_internal_cells(page_size);
            let (_temp, pager, mut tree, left_internal, right_internal, delete_key) =
                committed_three_level_tree(page_size, minimum_cells + 2, minimum_cells).unwrap();
            let mut batch = pager.begin_write().unwrap();
            assert!(tree.delete(&mut batch, &delete_key).unwrap().is_some());
            assert_eq!(
                internal_cell_count(&mut batch, left_internal),
                minimum_cells + 1
            );
            assert_eq!(
                internal_cell_count(&mut batch, right_internal),
                minimum_cells
            );
            assert_exact_separators(&mut batch, &tree).unwrap();

            let (_temp, pager, mut tree, left_internal, right_internal, _) =
                committed_three_level_tree(page_size, minimum_cells, minimum_cells + 2).unwrap();
            let delete_key = {
                let mut snapshot = pager.begin_read().unwrap();
                last_key_in_internal(&mut snapshot, TreeKind::Table, left_internal).unwrap()
            };
            let mut batch = pager.begin_write().unwrap();
            assert!(tree.delete(&mut batch, &delete_key).unwrap().is_some());
            assert_eq!(
                internal_cell_count(&mut batch, left_internal),
                minimum_cells
            );
            assert_eq!(
                internal_cell_count(&mut batch, right_internal),
                minimum_cells + 1
            );
            assert_exact_separators(&mut batch, &tree).unwrap();
        }
    }

    #[test]
    fn internal_merge_cascades_and_shrinks_root_at_every_page_size() {
        for page_size in [4096, 8192, 16384] {
            let minimum_cells = minimum_large_internal_cells(page_size);
            let (_temp, pager, mut tree, left_internal, right_internal, delete_key) =
                committed_three_level_tree(page_size, minimum_cells, minimum_cells).unwrap();
            let old_root = tree.root();
            let expected_key_count = 2 * (minimum_cells + 1) - 1;
            {
                let mut batch = pager.begin_write().unwrap();
                assert!(tree.delete(&mut batch, &delete_key).unwrap().is_some());
                assert_eq!(tree.root(), left_internal);
                assert_eq!(
                    internal_cell_count(&mut batch, left_internal),
                    2 * minimum_cells
                );
                assert_eq!(
                    scan_keys(&mut batch, &tree).unwrap().len(),
                    expected_key_count
                );
                assert_exact_separators(&mut batch, &tree).unwrap();
                batch.commit().unwrap();
            }
            let mut snapshot = pager.begin_read().unwrap();
            assert_eq!(tree.get(&mut snapshot, &delete_key).unwrap(), None);
            let replacements = {
                let mut batch = pager.begin_write().unwrap();
                let pages = (0..2)
                    .map(|_| batch.allocate(TreeKind::Table.leaf_page_type()).unwrap())
                    .collect::<Vec<_>>();
                for page in &pages {
                    SlottedPage::open(
                        batch.modify(*page).unwrap(),
                        super::super::LEAF_METADATA_END,
                    )
                    .unwrap();
                }
                batch.commit().unwrap();
                pages
            };
            let ids = replacements
                .iter()
                .map(|page| page.page_id())
                .collect::<HashSet<_>>();
            assert!(ids.contains(&right_internal.page_id()));
            assert!(ids.contains(&old_root.page_id()));
            assert_eq!(
                pager.read(right_internal).unwrap_err().code(),
                ErrorCode::Corruption
            );
            assert_eq!(
                pager.read(old_root).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn inserts_grow_root_and_keep_all_values_visible() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        let old_root = tree.root();
        for i in 0..600u32 {
            tree.insert(&mut batch, &i.to_be_bytes(), &[i as u8; 40])
                .unwrap();
        }
        assert_ne!(tree.root(), old_root);
        batch.commit().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        assert_eq!(
            tree.get(&mut snapshot, &599u32.to_be_bytes())
                .unwrap()
                .unwrap(),
            vec![599u32 as u8; 40]
        );
    }

    #[test]
    fn large_values_round_trip_through_overflow_pages() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let value = vec![0x5a; 9000];
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        tree.insert(&mut batch, b"large", &value).unwrap();
        batch.commit().unwrap();

        let mut snapshot = pager.begin_read().unwrap();
        assert_eq!(tree.get(&mut snapshot, b"large").unwrap(), Some(value));
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
        let pager = Pager::create(temp.path(), config).unwrap();
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
        let mut snapshot = pager.begin_read().unwrap();
        assert_eq!(
            tree.get(&mut snapshot, b"ok").unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(tree.get(&mut snapshot, b"too-large").unwrap(), None);
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
        let (_temp, pager) = test_pager(page_size)?;
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
        let mut snapshot = pager.begin_read()?;
        for value in order {
            assert_eq!(
                tree.get(&mut snapshot, &test_key(*value))?,
                Some(test_value(*value)),
                "{name} lookup failed at page size {page_size} for {value}"
            );
        }
        let mut scanned = Vec::new();
        let mut cursor = tree.scan(&mut snapshot, Bound::Unbounded, Bound::Unbounded)?;
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

    fn committed_two_leaf_tree(
        page_size: u32,
        left_count: u32,
        right_count: u32,
    ) -> Result<(TempDatabase, Pager, BTree, PageRef, PageRef)> {
        let (temp, pager) = test_pager(page_size)?;
        let mut batch = pager.begin_write()?;
        let left_ref = batch.allocate(TreeKind::Table.leaf_page_type())?;
        let right_ref = batch.allocate(TreeKind::Table.leaf_page_type())?;
        let root = batch.allocate(TreeKind::Table.internal_page_type())?;
        let value_len = leaf_test_value_len(page_size);
        let left = (0..left_count)
            .map(|index| LeafCell::inline(small_key(index), sized_value(index, value_len)))
            .collect::<Result<Vec<_>>>()?;
        let right = (left_count..left_count + right_count)
            .map(|index| LeafCell::inline(small_key(index), sized_value(index, value_len)))
            .collect::<Result<Vec<_>>>()?;
        rewrite_leaf(&mut batch, left_ref, Some(right_ref), &left)?;
        rewrite_leaf(&mut batch, right_ref, None, &right)?;
        rewrite_internal(
            &mut batch,
            root,
            left_ref,
            &[InternalCell::new(right[0].key().to_vec(), right_ref)?],
        )?;
        batch.commit()?;
        Ok((
            temp,
            pager,
            BTree {
                kind: TreeKind::Table,
                root,
            },
            left_ref,
            right_ref,
        ))
    }

    fn committed_three_leaf_tree(
        page_size: u32,
        left_count: u32,
        middle_count: u32,
        right_count: u32,
    ) -> Result<(TempDatabase, Pager, BTree, PageRef, PageRef, PageRef)> {
        let (temp, pager) = test_pager(page_size)?;
        let mut batch = pager.begin_write()?;
        let left_ref = batch.allocate(TreeKind::Table.leaf_page_type())?;
        let middle_ref = batch.allocate(TreeKind::Table.leaf_page_type())?;
        let right_ref = batch.allocate(TreeKind::Table.leaf_page_type())?;
        let root = batch.allocate(TreeKind::Table.internal_page_type())?;
        let value_len = leaf_test_value_len(page_size);
        let counts = [left_count, middle_count, right_count];
        let references = [left_ref, middle_ref, right_ref];
        let mut start = 0u32;
        let mut first_keys = Vec::new();
        for (index, count) in counts.into_iter().enumerate() {
            let cells = (start..start + count)
                .map(|value| LeafCell::inline(small_key(value), sized_value(value, value_len)))
                .collect::<Result<Vec<_>>>()?;
            first_keys.push(cells[0].key().to_vec());
            rewrite_leaf(
                &mut batch,
                references[index],
                references.get(index + 1).copied(),
                &cells,
            )?;
            start += count;
        }
        rewrite_internal(
            &mut batch,
            root,
            left_ref,
            &[
                InternalCell::new(first_keys[1].clone(), middle_ref)?,
                InternalCell::new(first_keys[2].clone(), right_ref)?,
            ],
        )?;
        batch.commit()?;
        Ok((
            temp,
            pager,
            BTree {
                kind: TreeKind::Table,
                root,
            },
            left_ref,
            middle_ref,
            right_ref,
        ))
    }

    fn committed_three_level_tree(
        page_size: u32,
        left_cells: usize,
        right_cells: usize,
    ) -> Result<(TempDatabase, Pager, BTree, PageRef, PageRef, Vec<u8>)> {
        let (temp, pager) = test_pager(page_size)?;
        let mut batch = pager.begin_write()?;
        let leaf_count = left_cells + right_cells + 2;
        let leaf_refs = (0..leaf_count)
            .map(|_| batch.allocate(TreeKind::Table.leaf_page_type()))
            .collect::<Result<Vec<_>>>()?;
        let value_len = usize::try_from(page_size)
            .expect("supported page size fits usize")
            .checked_sub(super::super::LEAF_METADATA_END)
            .ok_or_else(|| super::corruption("test page is shorter than leaf metadata"))?
            / 4;
        for (index, reference) in leaf_refs.iter().copied().enumerate() {
            let key = test_key(u32::try_from(index).unwrap());
            let value = sized_value(u32::try_from(index).unwrap(), value_len);
            let cell = LeafCell::inline(key, value)?;
            rewrite_leaf(
                &mut batch,
                reference,
                leaf_refs.get(index + 1).copied(),
                &[cell],
            )?;
        }

        let left_internal = batch.allocate(TreeKind::Table.internal_page_type())?;
        let right_internal = batch.allocate(TreeKind::Table.internal_page_type())?;
        let root = batch.allocate(TreeKind::Table.internal_page_type())?;
        let left_internal_cells = (1..=left_cells)
            .map(|index| {
                InternalCell::new(test_key(u32::try_from(index).unwrap()), leaf_refs[index])
            })
            .collect::<Result<Vec<_>>>()?;
        let right_start = left_cells + 1;
        let right_internal_cells = (1..=right_cells)
            .map(|offset| {
                let index = right_start + offset;
                InternalCell::new(test_key(u32::try_from(index).unwrap()), leaf_refs[index])
            })
            .collect::<Result<Vec<_>>>()?;
        rewrite_internal(
            &mut batch,
            left_internal,
            leaf_refs[0],
            &left_internal_cells,
        )?;
        rewrite_internal(
            &mut batch,
            right_internal,
            leaf_refs[right_start],
            &right_internal_cells,
        )?;
        let delete_key = test_key(u32::try_from(right_start).unwrap());
        rewrite_internal(
            &mut batch,
            root,
            left_internal,
            &[InternalCell::new(delete_key.clone(), right_internal)?],
        )?;
        batch.commit()?;
        Ok((
            temp,
            pager,
            BTree {
                kind: TreeKind::Table,
                root,
            },
            left_internal,
            right_internal,
            delete_key,
        ))
    }

    fn leaf_test_value_len(page_size: u32) -> usize {
        (usize::try_from(page_size).unwrap() - super::super::LEAF_METADATA_END) / 10 - 20
    }

    fn minimum_large_internal_cells(page_size: u32) -> usize {
        let usable = usize::try_from(page_size).unwrap() - super::super::INTERNAL_METADATA_END;
        let encoded_large_cell = 20 + 1024 + 4;
        usable.div_ceil(4).div_ceil(encoded_large_cell)
    }

    fn small_key(value: u32) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    fn sized_value(value: u32, len: usize) -> Vec<u8> {
        vec![value as u8; len]
    }

    fn leaf_keys(source: &mut impl PageSource, reference: PageRef) -> Vec<u32> {
        let Node::Leaf(leaf) = load_node(source, TreeKind::Table, reference).unwrap() else {
            panic!("expected leaf");
        };
        leaf.cells
            .iter()
            .map(|cell| u32::from_be_bytes(cell.key().try_into().unwrap()))
            .collect()
    }

    fn leaf_right_sibling(source: &mut impl PageSource, reference: PageRef) -> Option<PageRef> {
        let Node::Leaf(leaf) = load_node(source, TreeKind::Table, reference).unwrap() else {
            panic!("expected leaf");
        };
        leaf.right_sibling
    }

    fn scan_small_keys(source: &mut impl PageSource, tree: &BTree) -> Result<Vec<u32>> {
        scan_keys(source, tree)?
            .into_iter()
            .map(|key| {
                key.as_slice()
                    .try_into()
                    .map(u32::from_be_bytes)
                    .map_err(|_| super::corruption("small test key is not four bytes"))
            })
            .collect()
    }

    fn internal_cell_count(source: &mut impl PageSource, reference: PageRef) -> usize {
        let Node::Internal(internal) = load_node(source, TreeKind::Table, reference).unwrap()
        else {
            panic!("expected internal node");
        };
        internal.cells.len()
    }

    fn last_key_in_internal(
        source: &mut impl PageSource,
        kind: TreeKind,
        reference: PageRef,
    ) -> Result<Vec<u8>> {
        let Node::Internal(internal) = load_node(source, kind, reference)? else {
            return Err(super::corruption("test expected internal node"));
        };
        let last_child = internal
            .cells
            .last()
            .map(InternalCell::right)
            .unwrap_or(internal.leftmost);
        subtree_minimum(source, kind, last_child, &mut HashSet::new())
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
        let (_temp, pager) = test_pager(4096)?;
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
