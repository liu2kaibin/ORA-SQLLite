use crate::{DbError, ErrorCode, Result};

pub(super) const CATALOG_V1: u64 = 1 << 0;
pub(super) const KNOWN_FEATURE_BITS: u64 = CATALOG_V1;

pub(super) fn validate_feature_bits(bits: u64) -> Result<()> {
    if bits & !KNOWN_FEATURE_BITS != 0 {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "database state has unknown feature bits",
        ));
    }
    Ok(())
}
