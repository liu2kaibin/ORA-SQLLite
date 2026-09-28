//! Checked physical fences and an owned, lazy stream over IndexKey V1 entries.
use std::ops::Bound;

use super::{IndexStorage, corruption, key::IndexKeyCodec};
use crate::storage::{
    btree::BTreeScanState,
    catalog::types::{ColumnDef, ColumnType, RowId},
    pager::PageSource,
};
use crate::{DbError, ErrorCode, Result, Value};

pub(crate) struct IndexScanBound {
    pub(crate) value: Value,
    pub(crate) inclusive: bool,
}

/// Normalized leading equalities and at most one interval on the next column.
/// SQL NULL comparison semantics are handled by the caller, not the byte range.
pub(crate) struct IndexScanRange {
    pub(crate) equality: Box<[Value]>,
    pub(crate) lower: Option<IndexScanBound>,
    pub(crate) upper: Option<IndexScanBound>,
}

#[derive(Debug, PartialEq, Eq)]
enum BuiltRange {
    Empty,
    Bounds {
        start: Bound<Vec<u8>>,
        end: Bound<Vec<u8>>,
    },
}

impl IndexScanRange {
    fn build(&self, columns: &[ColumnDef]) -> Result<BuiltRange> {
        let equalities = self.equality.len();
        let has_range = self.lower.is_some() || self.upper.is_some();
        if equalities > columns.len() || (has_range && equalities == columns.len()) {
            return Err(DbError::new(
                ErrorCode::Type,
                "index scan exceeds index column count",
            ));
        }
        if has_range && columns[equalities].column_type() == ColumnType::Blob {
            return Err(DbError::new(
                ErrorCode::Type,
                "Blob index scans support equality only",
            ));
        }
        let equality = if equalities == 0 {
            Vec::new()
        } else {
            IndexKeyCodec::encode_prefix(&columns[..equalities], &self.equality)?
        };
        let encode_bound = |bound: &IndexScanBound| -> Result<(Vec<u8>, bool)> {
            let mut values = self.equality.to_vec();
            values.push(bound.value.clone());
            Ok((
                IndexKeyCodec::encode_prefix(&columns[..equalities + 1], &values)?,
                bound.inclusive,
            ))
        };
        Ok(build_physical_range(
            equality,
            self.lower.as_ref().map(encode_bound).transpose()?,
            self.upper.as_ref().map(encode_bound).transpose()?,
        ))
    }
}

/// Inputs are complete encoded component prefixes, without any ROWID suffix.
fn build_physical_range(
    equality: Vec<u8>,
    lower: Option<(Vec<u8>, bool)>,
    upper: Option<(Vec<u8>, bool)>,
) -> BuiltRange {
    let ceiling = IndexKeyCodec::prefix_successor(&equality);
    let start = match lower {
        Some((prefix, true)) => Some(prefix),
        Some((prefix, false)) => match IndexKeyCodec::prefix_successor(&prefix) {
            Some(successor) => Some(successor),
            None => return BuiltRange::Empty,
        },
        None if equality.is_empty() => None,
        None => Some(equality),
    };
    let upper = match upper {
        Some((prefix, true)) => IndexKeyCodec::prefix_successor(&prefix),
        Some((prefix, false)) => Some(prefix),
        None => None,
    };
    let end = match (ceiling, upper) {
        (Some(ceiling), Some(upper)) => Some(ceiling.min(upper)),
        (ceiling, upper) => ceiling.or(upper),
    };
    if matches!((&start, &end), (Some(start), Some(end)) if start >= end) {
        return BuiltRange::Empty;
    }
    BuiltRange::Bounds {
        start: start.map_or(Bound::Unbounded, Bound::Included),
        end: end.map_or(Bound::Unbounded, Bound::Excluded),
    }
}

pub(crate) struct ScannedIndexEntry {
    physical: Vec<u8>,
    rowid: RowId,
}

impl ScannedIndexEntry {
    pub(crate) const fn rowid(&self) -> RowId {
        self.rowid
    }
}

/// Retains one B+Tree cursor, never a collection of index keys or ROWIDs.
pub(crate) struct IndexRowIdScan {
    storage: IndexStorage,
    // None is explicitly empty; do not turn an empty interval into a full scan.
    state: Option<BTreeScanState>,
}

impl IndexStorage {
    pub(crate) fn scan_rowids(self, range: &IndexScanRange) -> Result<IndexRowIdScan> {
        let state = match range.build(&self.schema.columns)? {
            BuiltRange::Empty => None,
            BuiltRange::Bounds { start, end } => Some(self.tree.scan_state(
                start.as_ref().map(Vec::as_slice),
                end.as_ref().map(Vec::as_slice),
            )?),
        };
        Ok(IndexRowIdScan {
            storage: self,
            state,
        })
    }
}

impl IndexRowIdScan {
    #[allow(
        private_bounds,
        reason = "SQL callers pass concrete snapshots/batches; PageSource stays storage-private"
    )]
    pub(crate) fn next(
        &mut self,
        source: &mut impl PageSource,
    ) -> Result<Option<ScannedIndexEntry>> {
        let Some(state) = self.state.as_mut() else {
            return Ok(None);
        };
        let Some((physical, value)) = state.next(source)? else {
            return Ok(None);
        };
        if !value.is_empty() {
            return Err(corruption("index leaf value must be empty"));
        }
        let decoded = IndexKeyCodec::decode(&self.storage.schema.columns, &physical)?;
        Ok(Some(ScannedIndexEntry {
            physical,
            rowid: decoded.rowid(),
        }))
    }

    /// `values` is a complete table row; select indexed columns by table ordinal.
    pub(crate) fn validate_row(&self, entry: &ScannedIndexEntry, values: &[Value]) -> Result<()> {
        let encoded = self
            .storage
            .prepare(values, entry.rowid)
            .map_err(|_| corruption("table row cannot reproduce the scanned index key"))?;
        if encoded.physical != entry.physical {
            return Err(corruption("table row does not match the scanned index key"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        PageRef, Pager, WriteBatch,
        btree::{BTree, TreeKind},
        cache::PageHandle,
        catalog::types::{
            CatalogName, ColumnDef, ColumnId, ColumnType, IndexDef, IndexId, IndexOwnership, RowId,
            TableId,
        },
        index::{IndexSchema, IndexStorage, UniqueViolationName, key::IndexKeyCodec},
        pager::PageSource,
        test_support::{TempDatabase, test_pager},
    };
    use crate::{ErrorCode, Result, Value};
    use std::ops::Bound;

    struct RangeFixture {
        storage: IndexStorage,
        batch: WriteBatch,
        _pager: Pager,
        _temp: TempDatabase,
    }

    impl RangeFixture {
        fn new(types: &[ColumnType], rows: &[(Vec<Value>, u64)]) -> Self {
            let (temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
            let table = TableId::new(1).unwrap();
            let name = CatalogName::new("IX_RANGE").unwrap();
            let columns = types
                .iter()
                .enumerate()
                .map(|(ordinal, ty)| {
                    ColumnDef::new(
                        ColumnId::new(ordinal as u64 + 1).unwrap(),
                        table,
                        ordinal as u16,
                        *ty,
                        true,
                        CatalogName::new(format!("C{ordinal}")).unwrap(),
                    )
                    .unwrap()
                })
                .collect();
            let definition = IndexDef::new(
                IndexId::new(2).unwrap(),
                table,
                tree.root(),
                types.len() as u16,
                false,
                IndexOwnership::Explicit,
                name.clone(),
            )
            .unwrap();
            let mut storage = IndexStorage::from_new(
                IndexSchema {
                    definition,
                    columns,
                    unique_violation: UniqueViolationName::ExplicitIndex(name),
                },
                tree,
            );
            for (values, rowid) in rows {
                let entry = storage
                    .prepare(values, RowId::new(*rowid).unwrap())
                    .unwrap();
                storage.insert(&mut batch, &entry).unwrap();
            }
            Self {
                storage,
                batch,
                _pager: pager,
                _temp: temp,
            }
        }

        fn start(&self, range: IndexScanRange) -> Result<IndexRowIdScan> {
            IndexStorage::from_new(
                self.storage.schema.clone(),
                BTree::from_root(TreeKind::Index, self.storage.root()),
            )
            .scan_rowids(&range)
        }

        fn scan(
            &mut self,
            equality: Vec<Value>,
            lower: Option<IndexScanBound>,
            upper: Option<IndexScanBound>,
        ) -> Result<Vec<u64>> {
            let mut scan = self.start(IndexScanRange {
                equality: equality.into(),
                lower,
                upper,
            })?;
            let mut rowids = Vec::new();
            while let Some(entry) = scan.next(&mut self.batch)? {
                rowids.push(entry.rowid().get());
            }
            Ok(rowids)
        }
    }

    fn range_fixture(rows: &[([i64; 2], u64)]) -> RangeFixture {
        let rows: Vec<_> = rows
            .iter()
            .map(|(values, rowid)| (values.iter().copied().map(Value::Int64).collect(), *rowid))
            .collect();
        RangeFixture::new(&[ColumnType::Int64, ColumnType::Int64], &rows)
    }

    fn nullable_range_fixture() -> RangeFixture {
        RangeFixture::new(
            &[ColumnType::Int64, ColumnType::Int64],
            &[
                (vec![Value::Null, Value::Int64(1)], 1),
                (vec![Value::Null, Value::Int64(9)], 9),
                (vec![Value::Int64(-1), Value::Int64(4)], 2),
                (vec![Value::Int64(0), Value::Int64(5)], 3),
                (vec![Value::Int64(1), Value::Int64(6)], 4),
            ],
        )
    }

    fn eq<const N: usize>(values: [i64; N]) -> Vec<Value> {
        values.into_iter().map(Value::Int64).collect()
    }
    fn gt(value: i64) -> Option<IndexScanBound> {
        Some(IndexScanBound {
            value: Value::Int64(value),
            inclusive: false,
        })
    }
    fn ge(value: i64) -> Option<IndexScanBound> {
        Some(IndexScanBound {
            value: Value::Int64(value),
            inclusive: true,
        })
    }
    fn lt(value: i64) -> Option<IndexScanBound> {
        gt(value)
    }
    fn le(value: i64) -> Option<IndexScanBound> {
        ge(value)
    }
    fn unbounded() -> IndexScanRange {
        IndexScanRange {
            equality: Box::new([]),
            lower: None,
            upper: None,
        }
    }

    // A missing equality ceiling or the wrong successor inclusivity spills
    // into [2, 5], or retains/drops one of the duplicate [1, 5] ROWIDs.
    #[test]
    fn composite_range_fences_do_not_spill_into_adjacent_prefixes() {
        let mut fixture = range_fixture(&[
            ([1, 4], 1),
            ([1, 5], 2),
            ([1, 5], u64::MAX),
            ([1, 6], 3),
            ([2, 5], 4),
        ]);
        assert_eq!(fixture.scan(eq([1]), gt(5), le(6)).unwrap(), [3]);
        assert_eq!(fixture.scan(eq([1]), ge(5), lt(6)).unwrap(), [2, u64::MAX]);
        assert_eq!(fixture.scan(eq([1]), gt(5), None).unwrap(), [3]);
        assert_eq!(
            fixture.scan(eq([1]), None, lt(6)).unwrap(),
            [1, 2, u64::MAX]
        );
        assert_eq!(
            fixture.scan(eq([1]), None, None).unwrap(),
            [1, 2, u64::MAX, 3]
        );
        assert_eq!(fixture.scan(eq([1, 5]), None, None).unwrap(), [2, u64::MAX]);
    }

    // Encoding NULL as an unbounded prefix would include non-NULL rows.
    #[test]
    fn null_equality_and_unbounded_sides_have_exact_physical_bounds() {
        let mut fixture = nullable_range_fixture();
        assert_eq!(fixture.scan(vec![Value::Null], None, None).unwrap(), [1, 9]);
        assert_eq!(fixture.scan(vec![], None, lt(0)).unwrap(), [2]);
        assert_eq!(fixture.scan(vec![], ge(0), None).unwrap(), [3, 4, 1, 9]);
        assert_eq!(fixture.scan(vec![], None, None).unwrap(), [2, 3, 4, 1, 9]);
    }

    // Returning two unbounded sides for contradiction would scan every row.
    #[test]
    fn contradictory_and_equal_endpoint_ranges_are_empty() {
        let mut fixture = range_fixture(&[([1, 1], 1), ([1, 5], 2), ([1, 9], 3)]);
        assert!(fixture.scan(eq([1]), gt(9), lt(1)).unwrap().is_empty());
        assert!(fixture.scan(eq([1]), gt(5), le(5)).unwrap().is_empty());
        assert!(fixture.scan(eq([1]), ge(5), lt(5)).unwrap().is_empty());
        assert_eq!(fixture.scan(eq([1]), ge(5), le(5)).unwrap(), [2]);
    }

    // Valid V1 values start with 0/1, so exercise all-FF overflow at the
    // encoded-range boundary instead of fabricating an invalid typed value.
    #[test]
    fn strict_lower_successor_overflow_is_explicitly_empty() {
        assert_eq!(
            build_physical_range(vec![], Some((vec![255], false)), None),
            BuiltRange::Empty
        );
    }

    #[test]
    fn inclusive_upper_and_equality_successor_overflow_are_unbounded() {
        assert_eq!(
            build_physical_range(vec![], None, Some((vec![255], true))),
            BuiltRange::Bounds {
                start: Bound::Unbounded,
                end: Bound::Unbounded
            }
        );
        assert_eq!(
            build_physical_range(vec![255], None, None),
            BuiltRange::Bounds {
                start: Bound::Included(vec![255]),
                end: Bound::Unbounded
            }
        );
        assert_eq!(
            build_physical_range(vec![1], None, Some((vec![255], true))),
            BuiltRange::Bounds {
                start: Bound::Included(vec![1]),
                end: Bound::Excluded(vec![2])
            }
        );
    }

    // Literal encoded fenceposts check all four mappings independently.
    #[test]
    fn physical_bounds_have_exact_inclusive_exclusive_mappings() {
        assert_eq!(
            build_physical_range(vec![1], Some((vec![1, 5], true)), Some((vec![1, 6], false))),
            BuiltRange::Bounds {
                start: Bound::Included(vec![1, 5]),
                end: Bound::Excluded(vec![1, 6])
            }
        );
        assert_eq!(
            build_physical_range(vec![1], Some((vec![1, 5], false)), Some((vec![1, 6], true))),
            BuiltRange::Bounds {
                start: Bound::Included(vec![1, 6]),
                end: Bound::Excluded(vec![1, 7])
            }
        );
        assert_eq!(
            build_physical_range(vec![1], Some((vec![1, 255], false)), None),
            BuiltRange::Empty
        );
    }

    // Arity checks must precede slicing; Blob remains equality-only.
    #[test]
    fn excessive_arity_and_blob_range_requests_return_type_errors() {
        let mut fixture = range_fixture(&[]);
        assert_eq!(
            fixture.scan(eq([1, 2, 3]), None, None).unwrap_err().code(),
            ErrorCode::Type
        );
        assert_eq!(
            fixture.scan(eq([1, 2]), ge(3), None).unwrap_err().code(),
            ErrorCode::Type
        );
        assert_eq!(
            fixture.scan(eq([1, 2]), None, le(3)).unwrap_err().code(),
            ErrorCode::Type
        );
        let mut blobs =
            RangeFixture::new(&[ColumnType::Blob], &[(vec![Value::Blob(vec![0, 255])], 7)]);
        assert_eq!(
            blobs
                .scan(vec![Value::Blob(vec![0, 255])], None, None)
                .unwrap(),
            [7]
        );
        for inclusive in [false, true] {
            let bound = || {
                Some(IndexScanBound {
                    value: Value::Blob(vec![0, 255]),
                    inclusive,
                })
            };
            assert_eq!(
                blobs.scan(vec![], bound(), None).unwrap_err().code(),
                ErrorCode::Type
            );
            assert_eq!(
                blobs.scan(vec![], None, bound()).unwrap_err().code(),
                ErrorCode::Type
            );
        }
    }

    // Every returned physical entry must pass leaf-value and strict-key checks.
    #[test]
    fn non_empty_leaf_values_return_controlled_corruption() {
        let mut fixture = range_fixture(&[]);
        let entry = fixture
            .storage
            .prepare(&[Value::Int64(1), Value::Int64(5)], RowId::new(1).unwrap())
            .unwrap();
        fixture
            .storage
            .tree
            .insert(&mut fixture.batch, &entry.physical, &[7])
            .unwrap();
        assert_eq!(
            fixture.scan(vec![], None, None).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn malformed_keys_and_zero_rowid_return_controlled_corruption() {
        let mut fixture = range_fixture(&[]);
        let encoded = IndexKeyCodec::encode(
            &fixture.storage.schema.columns,
            &[Value::Int64(1), Value::Int64(5)],
            RowId::new(1).unwrap(),
        )
        .unwrap();
        let mut zero_rowid = encoded.physical().to_vec();
        zero_rowid[encoded.logical().len()..].fill(0);
        let mut trailing = encoded.physical().to_vec();
        trailing.push(1);
        for physical in [vec![2; 26], vec![0], zero_rowid, trailing] {
            fixture
                .storage
                .tree
                .insert(&mut fixture.batch, &physical, &[])
                .unwrap();
            assert_eq!(
                fixture.scan(vec![], None, None).unwrap_err().code(),
                ErrorCode::Corruption
            );
            fixture
                .storage
                .tree
                .delete(&mut fixture.batch, &physical)
                .unwrap();
        }
    }

    // Comparing only ROWIDs would silently accept an index pointing at a row
    // with different indexed values. Re-encoding must use table ordinals.
    #[test]
    fn row_validation_requires_exact_reencoded_physical_key() {
        let mut fixture = range_fixture(&[([1, 5], u64::MAX)]);
        fixture.storage.schema.columns = vec![
            ColumnDef::new(
                ColumnId::new(1).unwrap(),
                TableId::new(1).unwrap(),
                2,
                ColumnType::Int64,
                true,
                CatalogName::new("C2").unwrap(),
            )
            .unwrap(),
            ColumnDef::new(
                ColumnId::new(2).unwrap(),
                TableId::new(1).unwrap(),
                0,
                ColumnType::Int64,
                true,
                CatalogName::new("C0").unwrap(),
            )
            .unwrap(),
        ]
        .into();
        let mut scan = fixture.start(unbounded()).unwrap();
        let entry = scan.next(&mut fixture.batch).unwrap().unwrap();
        assert_eq!(entry.rowid().get(), u64::MAX);
        assert!(
            scan.validate_row(
                &entry,
                &[
                    Value::Int64(5),
                    Value::Text("unused".into()),
                    Value::Int64(1)
                ]
            )
            .is_ok()
        );
        for values in [
            vec![Value::Int64(6), Value::Null, Value::Int64(1)],
            vec![Value::Int64(5)],
            vec![
                Value::Text("wrong type".into()),
                Value::Null,
                Value::Int64(1),
            ],
        ] {
            assert_eq!(
                scan.validate_row(&entry, &values).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }

    struct CountingSource<'a> {
        batch: &'a mut WriteBatch,
        reads: usize,
    }

    impl PageSource for CountingSource<'_> {
        fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
            self.reads += 1;
            self.batch.read_page(reference)
        }
        fn page_count(&self) -> Result<u64> {
            self.batch.page_count()
        }
    }

    // Empty ranges do not even open the tree, and a single next() may not
    // eagerly fetch the rest of a multi-leaf index.
    #[test]
    fn empty_ranges_read_no_pages_and_rowids_stream_lazily() {
        let rows: Vec<_> = (0..600)
            .map(|value| ([1, value], value as u64 + 1))
            .collect();
        let mut fixture = range_fixture(&rows);
        let mut empty = fixture
            .start(IndexScanRange {
                equality: eq([1]).into(),
                lower: gt(9),
                upper: lt(1),
            })
            .unwrap();
        let mut scan = fixture.start(unbounded()).unwrap();
        let mut source = CountingSource {
            batch: &mut fixture.batch,
            reads: 0,
        };
        assert!(empty.next(&mut source).unwrap().is_none());
        assert!(empty.next(&mut source).unwrap().is_none());
        assert_eq!(source.reads, 0);
        assert_eq!(scan.next(&mut source).unwrap().unwrap().rowid().get(), 1);
        let first_reads = source.reads;
        assert!(first_reads > 0);
        for expected in 2..=600 {
            assert_eq!(
                scan.next(&mut source).unwrap().unwrap().rowid().get(),
                expected
            );
        }
        assert!(scan.next(&mut source).unwrap().is_none());
        assert!(
            source.reads > first_reads,
            "first next materialized every leaf"
        );
        let finished_reads = source.reads;
        assert!(scan.next(&mut source).unwrap().is_none());
        assert_eq!(source.reads, finished_reads);
    }
}
