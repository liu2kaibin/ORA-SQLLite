use sqlparser::tokenizer::{Location, Span};

use crate::error::SqlLocation;
use crate::sql::SqlSpan;
use crate::{DbError, ErrorCode, Result};

pub(super) struct SourceMap<'sql> {
    sql: &'sql str,
    line_starts: Vec<usize>,
}

impl<'sql> SourceMap<'sql> {
    pub(super) fn new(sql: &'sql str) -> Result<Self> {
        let mut line_starts = vec![0];
        for (offset, byte) in sql.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset.checked_add(1).ok_or_else(|| {
                    DbError::new(ErrorCode::Internal, "SQL source offset overflow")
                })?);
            }
        }
        Ok(Self { sql, line_starts })
    }

    pub(super) fn location(&self, byte_offset: usize) -> Result<SqlLocation> {
        if byte_offset > self.sql.len() || !self.sql.is_char_boundary(byte_offset) {
            return Err(invalid_source_location());
        }

        let line_index = self
            .line_starts
            .partition_point(|line_start| *line_start <= byte_offset)
            .checked_sub(1)
            .ok_or_else(invalid_source_location)?;
        let line_start = self.line_starts[line_index];
        let line = line_index
            .checked_add(1)
            .ok_or_else(invalid_source_location)?;
        let column = self.sql[line_start..byte_offset]
            .chars()
            .count()
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
        let line_start = *self
            .line_starts
            .get(line_index)
            .ok_or_else(invalid_source_location)?;
        let is_last_line = line_index + 1 == self.line_starts.len();
        let line_end = self
            .line_starts
            .get(line_index + 1)
            .copied()
            .unwrap_or(self.sql.len());
        let line_text = &self.sql[line_start..line_end];

        if column_index == 0 {
            return Ok(line_start);
        }

        let character_count = line_text.chars().count();
        if column_index > character_count || (!is_last_line && column_index == character_count) {
            return Err(invalid_source_location());
        }

        line_text
            .char_indices()
            .nth(column_index)
            .map(|(offset, _)| line_start + offset)
            .or_else(|| is_last_line.then_some(line_end))
            .ok_or_else(invalid_source_location)
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
}
