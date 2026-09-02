use crate::{DbError, ErrorCode, Result};

const MAX_MAGNITUDE: u128 = 99_999_999_999_999_999_999_999_999_999_999_999_999;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    negative: bool,
    scale: i8,
    magnitude: u128,
}

impl Decimal {
    pub fn new(negative: bool, scale: i8, magnitude: u128) -> Result<Self> {
        if !(-38..=38).contains(&scale) {
            return Err(DbError::new(
                ErrorCode::Type,
                "decimal scale must be between -38 and 38",
            ));
        }

        if magnitude > MAX_MAGNITUDE {
            return Err(DbError::new(
                ErrorCode::Type,
                "decimal magnitude must contain at most 38 digits",
            ));
        }

        Ok(Self {
            negative: negative && magnitude != 0,
            scale,
            magnitude,
        })
    }

    pub const fn negative(&self) -> bool {
        self.negative
    }

    pub const fn scale(&self) -> i8 {
        self.scale
    }

    pub const fn magnitude(&self) -> u128 {
        self.magnitude
    }
}

#[cfg(test)]
mod tests {
    use super::Decimal;
    use crate::ErrorCode;

    #[test]
    fn accepts_38_digit_magnitude() {
        let value = Decimal::new(false, 2, 99_999_999_999_999_999_999_999_999_999_999_999_999);
        assert!(value.is_ok());
    }

    #[test]
    fn rejects_more_than_38_digits_and_invalid_scale() {
        assert!(
            Decimal::new(
                false,
                0,
                100_000_000_000_000_000_000_000_000_000_000_000_000
            )
            .is_err()
        );
        assert!(Decimal::new(false, 39, 1).is_err());
    }

    #[test]
    fn normalizes_negative_zero_and_exposes_components() {
        let value = Decimal::new(true, -38, 0).expect("valid decimal");
        let negative = Decimal::new(true, 2, 123).expect("valid decimal");

        assert!(!value.negative());
        assert_eq!(value.scale(), -38);
        assert_eq!(value.magnitude(), 0);
        assert!(negative.negative());
        assert_eq!(negative.scale(), 2);
        assert_eq!(negative.magnitude(), 123);
    }

    #[test]
    fn accepts_scale_boundaries_and_reports_type_errors() {
        assert!(Decimal::new(false, 38, 1).is_ok());
        let error = Decimal::new(false, -39, 1).expect_err("scale below minimum");
        let magnitude_error = Decimal::new(
            false,
            0,
            100_000_000_000_000_000_000_000_000_000_000_000_000,
        )
        .expect_err("magnitude above maximum");

        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(magnitude_error.code(), ErrorCode::Type);
    }
}
