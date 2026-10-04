use crate::{DbError, ErrorCode, Result};

pub(super) const CATALOG_V1: u64 = 1 << 0;
pub(super) const INDEX_CONSTRAINT_V1: u64 = 1 << 1;
pub(super) const ORACLE_TYPES_V1: u64 = 1 << 2;
pub(super) const KNOWN_FEATURE_BITS: u64 = CATALOG_V1 | INDEX_CONSTRAINT_V1 | ORACLE_TYPES_V1;

pub(super) fn validate_feature_bits(bits: u64) -> Result<()> {
    if bits & !KNOWN_FEATURE_BITS != 0 {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "database uses unsupported feature bits",
        ));
    }
    if bits & INDEX_CONSTRAINT_V1 != 0 && bits & CATALOG_V1 == 0 {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "index/constraint feature requires catalog V1",
        ));
    }
    if bits & ORACLE_TYPES_V1 != 0 && bits & CATALOG_V1 == 0 {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "Oracle type feature requires catalog V1",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_4c2_feature_bit_is_known_and_requires_catalog_v1() {
        assert_eq!(INDEX_CONSTRAINT_V1, 1 << 1);
        assert_eq!(
            KNOWN_FEATURE_BITS,
            CATALOG_V1 | INDEX_CONSTRAINT_V1 | ORACLE_TYPES_V1
        );
        assert!(validate_feature_bits(CATALOG_V1 | INDEX_CONSTRAINT_V1).is_ok());
        assert_eq!(
            validate_feature_bits(INDEX_CONSTRAINT_V1)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            validate_feature_bits(1 << 63).unwrap_err().code(),
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn oracle_type_feature_requires_catalog_v1() {
        assert_eq!(ORACLE_TYPES_V1, 1 << 2);
        assert!(validate_feature_bits(CATALOG_V1 | ORACLE_TYPES_V1).is_ok());
        assert_eq!(
            validate_feature_bits(ORACLE_TYPES_V1).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }
}
