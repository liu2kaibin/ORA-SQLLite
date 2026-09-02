use super::{
    PageId, PageRef,
    codec::{Decoder, Encoder},
    page::PageType,
};
use crate::{DbError, ErrorCode, Result};

mod mutate;
mod search;

#[allow(
    unused_imports,
    reason = "the B+Tree search facade is consumed by subsequent storage tasks"
)]
pub(super) use search::{BTree, BTreeCursor};

pub(super) const LEAF_METADATA_END: usize = 56;
pub(super) const INTERNAL_METADATA_END: usize = 64;
const MAX_KEY_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeKind {
    Table,
    Index,
}

impl TreeKind {
    pub(super) const fn leaf_page_type(self) -> PageType {
        match self {
            Self::Table => PageType::TableLeaf,
            Self::Index => PageType::IndexLeaf,
        }
    }

    pub(super) const fn internal_page_type(self) -> PageType {
        match self {
            Self::Table => PageType::TableInternal,
            Self::Index => PageType::IndexInternal,
        }
    }
}

pub(super) fn encode_leaf_right_sibling(right_sibling: Option<PageRef>) -> [u8; 8] {
    right_sibling.map_or(0, PageRef::generation).to_le_bytes()
}

pub(super) fn decode_leaf_right_sibling(
    bytes: &[u8],
    right_sibling: Option<PageId>,
) -> Result<Option<PageRef>> {
    let generation = u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| corruption("leaf metadata must be exactly 8 bytes"))?,
    );
    match (right_sibling, generation) {
        (None, 0) => Ok(None),
        (None, _) => Err(corruption(
            "leaf metadata has a generation without a right sibling",
        )),
        (Some(_), 0) => Err(corruption("right sibling is missing its generation")),
        (Some(page_id), generation) => Ok(Some(PageRef::new(page_id, generation)?)),
    }
}

pub(super) fn encode_internal_leftmost(leftmost: PageRef) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&leftmost.page_id().0.to_le_bytes());
    bytes[8..].copy_from_slice(&leftmost.generation().to_le_bytes());
    bytes
}

pub(super) fn decode_internal_leftmost(bytes: &[u8]) -> Result<PageRef> {
    let bytes: &[u8; 16] = bytes
        .try_into()
        .map_err(|_| corruption("internal metadata must be exactly 16 bytes"))?;
    let page_id =
        PageId(u64::from_le_bytes(bytes[..8].try_into().map_err(|_| {
            corruption("internal metadata page id is truncated")
        })?));
    let generation = u64::from_le_bytes(
        bytes[8..]
            .try_into()
            .map_err(|_| corruption("internal metadata generation is truncated"))?,
    );
    PageRef::new(page_id, generation)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ValueStorage {
    Inline(Vec<u8>),
    Overflow(PageRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LeafCell {
    key: Vec<u8>,
    storage: ValueStorage,
    total_value_len: u64,
}

impl LeafCell {
    pub(super) fn inline(key: Vec<u8>, value: Vec<u8>) -> Result<Self> {
        validate_key(&key)?;
        let total_value_len = u64::try_from(value.len())
            .map_err(|_| type_error("inline value length does not fit the disk format"))?;
        Ok(Self {
            key,
            storage: ValueStorage::Inline(value),
            total_value_len,
        })
    }

    pub(super) fn overflow(
        key: Vec<u8>,
        total_value_len: u64,
        first_overflow: PageRef,
    ) -> Result<Self> {
        validate_key(&key)?;
        if total_value_len == 0 {
            return Err(type_error("empty values must use inline storage"));
        }
        Ok(Self {
            key,
            storage: ValueStorage::Overflow(first_overflow),
            total_value_len,
        })
    }

    pub(super) fn key(&self) -> &[u8] {
        &self.key
    }

    pub(super) const fn total_value_len(&self) -> u64 {
        self.total_value_len
    }

    pub(super) fn storage(&self) -> &ValueStorage {
        &self.storage
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        validate_key(&self.key)?;
        let key_len = u16::try_from(self.key.len())
            .map_err(|_| type_error("key length does not fit the disk format"))?;
        let mut encoder = Encoder::new();
        encoder.put_u16(key_len);
        match &self.storage {
            ValueStorage::Inline(value) => {
                let value_len = u64::try_from(value.len())
                    .map_err(|_| type_error("inline value length does not fit the disk format"))?;
                if value_len != self.total_value_len {
                    return Err(corruption(
                        "inline value length does not match its total length",
                    ));
                }
                encoder.put_u8(0);
                encoder.put_u8(0);
                encoder.put_u64(self.total_value_len);
                encoder.put_bytes(&self.key);
                encoder.put_bytes(value);
            }
            ValueStorage::Overflow(first_overflow) => {
                if self.total_value_len == 0 {
                    return Err(corruption("empty values must use inline storage"));
                }
                encoder.put_u8(1);
                encoder.put_u8(0);
                encoder.put_u64(self.total_value_len);
                encoder.put_bytes(&self.key);
                encoder.put_u64(first_overflow.page_id().0);
                encoder.put_u64(first_overflow.generation());
            }
        }
        Ok(encoder.into_bytes())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(bytes);
        let key_len = usize::from(decoder.get_u16()?);
        let storage_code = decoder.get_u8()?;
        let reserved = decoder.get_u8()?;
        let total_value_len = decoder.get_u64()?;
        if key_len > MAX_KEY_LEN {
            return Err(corruption("leaf cell key exceeds the maximum length"));
        }
        if reserved != 0 {
            return Err(corruption("leaf cell reserved byte is nonzero"));
        }
        let key = decoder.get_bytes(key_len)?.to_vec();

        let storage = match storage_code {
            0 => {
                let value_len = usize::try_from(total_value_len)
                    .map_err(|_| corruption("inline value length exceeds this platform"))?;
                let value = decoder.get_bytes(value_len)?.to_vec();
                if decoder.remaining() != 0 {
                    return Err(corruption("leaf inline cell has trailing bytes"));
                }
                ValueStorage::Inline(value)
            }
            1 => {
                if total_value_len == 0 {
                    return Err(corruption("empty values must use inline storage"));
                }
                let page_id = PageId(decoder.get_u64()?);
                let generation = decoder.get_u64()?;
                if decoder.remaining() != 0 {
                    return Err(corruption("leaf overflow cell has trailing bytes"));
                }
                ValueStorage::Overflow(PageRef::new(page_id, generation)?)
            }
            _ => return Err(corruption("leaf cell has an unknown storage code")),
        };
        Ok(Self {
            key,
            storage,
            total_value_len,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InternalCell {
    key: Vec<u8>,
    right: PageRef,
}

impl InternalCell {
    pub(super) fn new(key: Vec<u8>, right: PageRef) -> Result<Self> {
        validate_key(&key)?;
        Ok(Self { key, right })
    }

    pub(super) fn key(&self) -> &[u8] {
        &self.key
    }

    pub(super) const fn right(&self) -> PageRef {
        self.right
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        validate_key(&self.key)?;
        let key_len = u16::try_from(self.key.len())
            .map_err(|_| type_error("key length does not fit the disk format"))?;
        let mut encoder = Encoder::new();
        encoder.put_u16(key_len);
        encoder.put_u16(0);
        encoder.put_u64(self.right.page_id().0);
        encoder.put_u64(self.right.generation());
        encoder.put_bytes(&self.key);
        Ok(encoder.into_bytes())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(bytes);
        let key_len = usize::from(decoder.get_u16()?);
        let reserved = decoder.get_u16()?;
        let page_id = PageId(decoder.get_u64()?);
        let generation = decoder.get_u64()?;
        if key_len > MAX_KEY_LEN {
            return Err(corruption("internal cell key exceeds the maximum length"));
        }
        if reserved != 0 {
            return Err(corruption("internal cell reserved bytes are nonzero"));
        }
        let key = decoder.get_bytes(key_len)?.to_vec();
        if decoder.remaining() != 0 {
            return Err(corruption("internal cell has trailing bytes"));
        }
        Ok(Self {
            key,
            right: PageRef::new(page_id, generation)?,
        })
    }
}

fn validate_key(key: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY_LEN {
        return Err(type_error("B+Tree keys must be at most 1024 bytes"));
    }
    Ok(())
}

fn type_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message)
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::{
        InternalCell, LeafCell, TreeKind, decode_internal_leftmost, decode_leaf_right_sibling,
        encode_internal_leftmost, encode_leaf_right_sibling,
    };
    use crate::ErrorCode;

    use super::{
        super::{PageId, PageRef, page::PageType, test_support::test_pager},
        search::BTree,
    };

    #[test]
    fn empty_tree_is_one_leaf_and_scans_empty() {
        let (_temp, mut pager) = test_pager(4096).expect("test pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin write");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("create empty tree");
            batch.commit().expect("commit empty tree");
            tree
        };

        let root = pager.read(tree.root()).expect("read root page");
        assert_eq!(root.borrow().header().page_type(), PageType::TableLeaf);

        assert_eq!(
            tree.scan(&mut pager, Bound::Unbounded, Bound::Unbounded)
                .expect("scan empty tree")
                .next()
                .expect("read empty cursor"),
            None
        );
    }

    #[test]
    fn cell_codecs_round_trip_and_enforce_key_limit() {
        let leaf = LeafCell::inline(b"K".to_vec(), b"value".to_vec()).expect("inline leaf cell");
        assert_eq!(
            LeafCell::decode(&leaf.encode().expect("encode leaf")).expect("decode leaf"),
            leaf
        );

        let child = PageRef::new(PageId(9), 2).expect("page reference");
        let internal = InternalCell::new(b"M".to_vec(), child).expect("internal cell");
        assert_eq!(
            InternalCell::decode(&internal.encode().expect("encode internal"))
                .expect("decode internal"),
            internal
        );

        assert_eq!(
            LeafCell::inline(vec![0; 1025], vec![])
                .expect_err("key limit")
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn leaf_cells_use_the_fixed_little_endian_inline_and_overflow_encodings() {
        let inline = LeafCell::inline(b"K".to_vec(), b"value".to_vec()).expect("inline leaf");
        assert_eq!(
            inline.encode().expect("inline encoding"),
            vec![
                1, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, b'K', b'v', b'a', b'l', b'u', b'e',
            ]
        );

        let overflow = LeafCell::overflow(
            b"K".to_vec(),
            0x0102_0304_0506_0708,
            PageRef::new(PageId(9), 2).expect("overflow page reference"),
        )
        .expect("overflow leaf");
        assert_eq!(
            overflow.encode().expect("overflow encoding"),
            vec![
                1, 0, 1, 0, 8, 7, 6, 5, 4, 3, 2, 1, b'K', 9, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0,
                0, 0,
            ]
        );
    }

    #[test]
    fn internal_cells_use_the_fixed_little_endian_encoding() {
        let cell = InternalCell::new(
            b"M".to_vec(),
            PageRef::new(PageId(9), 2).expect("right child reference"),
        )
        .expect("internal cell");

        assert_eq!(
            cell.encode().expect("internal encoding"),
            vec![
                1, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, b'M',
            ]
        );
    }

    #[test]
    fn node_metadata_round_trips_at_its_exact_fixed_offsets() {
        let sibling = PageRef::new(PageId(9), 2).expect("sibling reference");
        assert_eq!(
            encode_leaf_right_sibling(Some(sibling)),
            [2, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            decode_leaf_right_sibling(&[2, 0, 0, 0, 0, 0, 0, 0], Some(PageId(9)))
                .expect("decode sibling"),
            Some(sibling)
        );
        assert_eq!(encode_leaf_right_sibling(None), [0; 8]);
        assert_eq!(
            decode_leaf_right_sibling(&[0; 8], None).expect("decode absent sibling"),
            None
        );

        let leftmost = PageRef::new(PageId(7), 3).expect("leftmost reference");
        assert_eq!(
            encode_internal_leftmost(leftmost),
            [7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            decode_internal_leftmost(&[7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0,])
                .expect("decode leftmost"),
            leftmost
        );
    }

    #[test]
    fn node_metadata_rejects_wrong_lengths_and_sibling_presence_mismatches() {
        assert_eq!(
            decode_leaf_right_sibling(&[0; 8], Some(PageId(9)))
                .expect_err("present sibling needs a generation")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            decode_leaf_right_sibling(&[2, 0, 0, 0, 0, 0, 0, 0], None)
                .expect_err("absent sibling needs zero generation")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            decode_internal_leftmost(&[0; 15])
                .expect_err("truncated internal metadata")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn tree_kinds_route_to_their_exact_leaf_and_internal_page_types() {
        assert_eq!(TreeKind::Table.leaf_page_type() as u8, 3);
        assert_eq!(TreeKind::Table.internal_page_type() as u8, 2);
        assert_eq!(TreeKind::Index.leaf_page_type() as u8, 5);
        assert_eq!(TreeKind::Index.internal_page_type() as u8, 4);
    }

    #[test]
    fn malformed_cells_are_corruption_and_overflow_must_have_a_value() {
        assert_eq!(
            LeafCell::decode(&[1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, b'K'])
                .expect_err("nonzero reserved byte")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            LeafCell::decode(&[1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, b'K', b'v', 0])
                .expect_err("inline trailing byte")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            LeafCell::decode(&[
                1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, b'K', 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0,
                0, 0,
            ])
            .expect_err("zero overflow page")
            .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            InternalCell::decode(&[
                1, 0, 1, 0, 9, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, b'M',
            ])
            .expect_err("nonzero internal reserved bytes")
            .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            LeafCell::overflow(
                b"K".to_vec(),
                0,
                PageRef::new(PageId(9), 2).expect("page reference"),
            )
            .expect_err("empty value must remain inline")
            .code(),
            ErrorCode::Type
        );
    }
}
