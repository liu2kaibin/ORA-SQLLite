#[cfg(test)]
use std::cell::Cell;

use sqlparser::tokenizer::{Location, Span};

use crate::error::SqlLocation;
use crate::sql::SqlSpan;
use crate::{DbError, ErrorCode, Result};

pub(super) struct SourceMap<'sql> {
    sql: &'sql str,
    line_boundary_starts: Vec<usize>,
    scalar_boundaries: Vec<usize>,
    #[cfg(test)]
    lookup_scalar_visits: Cell<usize>,
}

impl<'sql> SourceMap<'sql> {
    pub(super) fn new(sql: &'sql str) -> Result<Self> {
        let mut line_boundary_starts = vec![0];
        let mut scalar_boundaries = vec![0];
        for (offset, character) in sql.char_indices() {
            let next_offset = offset
                .checked_add(character.len_utf8())
                .ok_or_else(|| DbError::new(ErrorCode::Internal, "SQL source offset overflow"))?;
            if character == '\n' {
                line_boundary_starts.push(scalar_boundaries.len());
            }
            scalar_boundaries.push(next_offset);
        }
        Ok(Self {
            sql,
            line_boundary_starts,
            scalar_boundaries,
            #[cfg(test)]
            lookup_scalar_visits: Cell::new(0),
        })
    }

    pub(super) fn location(&self, byte_offset: usize) -> Result<SqlLocation> {
        if byte_offset > self.sql.len() || !self.sql.is_char_boundary(byte_offset) {
            return Err(invalid_source_location());
        }

        let line_index = self
            .line_boundary_starts
            .partition_point(|boundary_start| {
                self.scalar_boundaries[*boundary_start] <= byte_offset
            })
            .checked_sub(1)
            .ok_or_else(invalid_source_location)?;
        let line = line_index
            .checked_add(1)
            .ok_or_else(invalid_source_location)?;
        let column = self
            .line_boundaries(line_index)?
            .binary_search(&byte_offset)
            .map_err(|_| invalid_source_location())?
            .checked_add(1)
            .ok_or_else(invalid_source_location)?;
        SqlLocation::new(byte_offset, line, column)
    }

    pub(super) fn span(&self, span: Span) -> Result<SqlSpan> {
        let start = self.byte_offset(span.start)?;
        let end = self.byte_offset(span.end)?;
        SqlSpan::new(start, end)
    }

    pub(super) fn sqlparser_location(&self, location: Location) -> Result<SqlLocation> {
        self.location(self.byte_offset(location)?)
    }

    fn byte_offset(&self, location: Location) -> Result<usize> {
        let line = usize::try_from(location.line).map_err(|_| invalid_source_location())?;
        let column = usize::try_from(location.column).map_err(|_| invalid_source_location())?;
        let line_index = line.checked_sub(1).ok_or_else(invalid_source_location)?;
        let column_index = column.checked_sub(1).ok_or_else(invalid_source_location)?;
        let columns = self.line_boundaries(line_index)?;
        #[cfg(test)]
        self.lookup_scalar_visits.set(
            self.lookup_scalar_visits
                .get()
                .checked_add(1)
                .expect("test lookup counter must not overflow"),
        );
        columns
            .get(column_index)
            .copied()
            .ok_or_else(invalid_source_location)
    }

    fn line_boundaries(&self, line_index: usize) -> Result<&[usize]> {
        let start = *self
            .line_boundary_starts
            .get(line_index)
            .ok_or_else(invalid_source_location)?;
        let end = self
            .line_boundary_starts
            .get(line_index + 1)
            .copied()
            .unwrap_or(self.scalar_boundaries.len());
        self.scalar_boundaries
            .get(start..end)
            .ok_or_else(invalid_source_location)
    }

    #[cfg(test)]
    fn lookup_scalar_visits(&self) -> usize {
        self.lookup_scalar_visits.get()
    }

    #[cfg(test)]
    fn growable_buffer_count(&self) -> usize {
        usize::from(self.line_boundary_starts.capacity() != 0)
            + usize::from(self.scalar_boundaries.capacity() != 0)
    }
}

fn invalid_source_location() -> DbError {
    DbError::new(
        ErrorCode::Internal,
        "SQL source location is outside the original text",
    )
}

#[cfg(test)]
mod tests {
    use sqlparser::tokenizer::{Location, Span};

    use super::SourceMap;
    use crate::error::SqlLocation;
    use crate::sql::SqlSpan;

    #[test]
    fn source_map_treats_crlf_as_one_line_break() {
        let map = SourceMap::new("一\r\n:x").unwrap();
        assert_eq!(map.location(5).unwrap(), SqlLocation::new(5, 2, 1).unwrap());
    }

    #[test]
    fn source_map_counts_unicode_scalars_and_lf_lines() {
        let map = SourceMap::new("一x\ny").unwrap();
        assert_eq!(map.location(3).unwrap(), SqlLocation::new(3, 1, 2).unwrap());
        assert_eq!(map.location(5).unwrap(), SqlLocation::new(5, 2, 1).unwrap());
        assert_eq!(map.location(6).unwrap(), SqlLocation::new(6, 2, 2).unwrap());
    }

    #[test]
    fn source_map_converts_sqlparser_exclusive_spans_to_byte_spans() {
        let map = SourceMap::new("一x\ny").unwrap();
        let upstream = Span::new(Location::new(1, 2), Location::new(2, 2));
        assert_eq!(map.span(upstream).unwrap(), SqlSpan::new(3, 6).unwrap());
    }

    #[test]
    fn source_map_does_not_rescan_scalar_prefixes_for_span_lookups() {
        let sql = "界".repeat(256);
        let map = SourceMap::new(&sql).unwrap();

        for column in 1..=256 {
            let span = Span::new(Location::new(1, column), Location::new(1, column + 1));
            map.span(span).unwrap();
        }

        assert_eq!(map.lookup_scalar_visits(), 512);
    }

    #[test]
    fn source_map_storage_does_not_allocate_a_buffer_per_line() {
        let sql = "\n".repeat(256);
        let map = SourceMap::new(&sql).unwrap();

        assert!(
            map.growable_buffer_count() <= 3,
            "expected at most 3 growable buffers, got {}",
            map.growable_buffer_count()
        );
    }
}
