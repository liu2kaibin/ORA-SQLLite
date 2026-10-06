use crate::{DbError, ErrorCode, Result};

const MAX_MAGNITUDE: u128 = 99_999_999_999_999_999_999_999_999_999_999_999_999;

/// An exact decimal with up to 38 magnitude digits and a scale from -38 through 38.
///
/// The represented value is `magnitude × 10^-scale`, negated when [`Decimal::negative`] is true.
/// Zero is always normalized to non-negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    negative: bool,
    scale: i8,
    magnitude: u128,
}

impl Decimal {
    /// Constructs a checked decimal from its sign, scale, and unsigned magnitude.
    ///
    /// Returns [`ErrorCode::Type`] if `scale` is outside `-38..=38` or `magnitude` contains more
    /// than 38 decimal digits. A zero magnitude clears `negative`.
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

    /// Returns whether the nonzero value is negative.
    pub const fn negative(&self) -> bool {
        self.negative
    }

    /// Returns the base-10 scale in the inclusive range `-38..=38`.
    pub const fn scale(&self) -> i8 {
        self.scale
    }

    /// Returns the unsigned coefficient, which contains at most 38 decimal digits.
    pub const fn magnitude(&self) -> u128 {
        self.magnitude
    }

    #[allow(
        dead_code,
        reason = "Oracle normalization is wired into execution by later phase 5 tasks"
    )]
    pub(crate) fn parse_ascii_quantized(text: &str, precision: u8, scale: i8) -> Result<Self> {
        let bytes = text.as_bytes();
        let (negative, body) = match bytes.first() {
            Some(b'+') => (false, &bytes[1..]),
            Some(b'-') => (true, &bytes[1..]),
            _ => (false, bytes),
        };
        if body.is_empty() {
            return Err(invalid_number(precision, scale));
        }

        let mut saw_digit = false;
        let mut saw_point = false;
        let mut digit_count = 0_usize;
        let mut leading_zeros = 0_usize;
        let mut saw_nonzero = false;
        let mut fractional_digits = 0_usize;
        for &byte in body {
            match byte {
                b'0'..=b'9' => {
                    saw_digit = true;
                    digit_count = digit_count
                        .checked_add(1)
                        .ok_or_else(|| invalid_number(precision, scale))?;
                    if !saw_nonzero {
                        if byte == b'0' {
                            leading_zeros = leading_zeros
                                .checked_add(1)
                                .ok_or_else(|| invalid_number(precision, scale))?;
                        } else {
                            saw_nonzero = true;
                        }
                    }
                    if saw_point {
                        fractional_digits = fractional_digits
                            .checked_add(1)
                            .ok_or_else(|| invalid_number(precision, scale))?;
                    }
                }
                b'.' if !saw_point => saw_point = true,
                _ => return Err(invalid_number(precision, scale)),
            }
        }
        if !saw_digit {
            return Err(invalid_number(precision, scale));
        }

        let significant_digits = digit_count - leading_zeros;
        if significant_digits == 0 {
            return Decimal::new(false, scale, 0);
        }
        let (kept_digits, appended_zeros, rounding_index) = if scale >= 0 {
            let target_fractional = usize::from(scale.unsigned_abs());
            if target_fractional >= fractional_digits {
                (
                    significant_digits,
                    target_fractional - fractional_digits,
                    None,
                )
            } else {
                discard_shape(significant_digits, fractional_digits - target_fractional)
            }
        } else {
            let discarded = fractional_digits
                .checked_add(usize::from(scale.unsigned_abs()))
                .ok_or_else(|| invalid_number(precision, scale))?;
            discard_shape(significant_digits, discarded)
        };
        if kept_digits.saturating_add(appended_zeros) > usize::from(precision) {
            return Err(precision_overflow(precision, scale));
        }

        let mut skipped_zeros = 0_usize;
        let mut significant_index = 0_usize;
        let mut magnitude = 0_u128;
        let mut round_up = false;
        for &byte in body {
            let b'0'..=b'9' = byte else {
                continue;
            };
            if skipped_zeros < leading_zeros {
                skipped_zeros += 1;
                continue;
            }
            let digit = byte - b'0';
            if significant_index < kept_digits {
                magnitude = magnitude
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u128::from(digit)))
                    .ok_or_else(|| precision_overflow(precision, scale))?;
            } else if rounding_index == Some(significant_index) {
                round_up = digit >= 5;
                break;
            }
            significant_index += 1;
        }
        if appended_zeros != 0 {
            let exponent =
                u32::try_from(appended_zeros).map_err(|_| precision_overflow(precision, scale))?;
            magnitude = magnitude
                .checked_mul(
                    10_u128
                        .checked_pow(exponent)
                        .ok_or_else(|| precision_overflow(precision, scale))?,
                )
                .ok_or_else(|| precision_overflow(precision, scale))?;
        }
        if round_up {
            magnitude = magnitude
                .checked_add(1)
                .ok_or_else(|| precision_overflow(precision, scale))?;
        }
        finish_quantized(negative, magnitude, precision, scale)
    }

    #[allow(
        dead_code,
        reason = "Oracle normalization is wired into execution by later phase 5 tasks"
    )]
    pub(crate) fn quantize(self, precision: u8, scale: i8) -> Result<Self> {
        if self.magnitude == 0 {
            return Self::new(false, scale, 0);
        }
        let digits = self
            .magnitude
            .to_string()
            .bytes()
            .map(|byte| byte - b'0')
            .collect::<Vec<_>>();
        let scale_difference = i16::from(scale) - i16::from(self.scale);
        let shift = if scale_difference >= 0 {
            DecimalShift::Append(usize::try_from(scale_difference).expect("nonnegative i16"))
        } else {
            DecimalShift::Discard(usize::from(scale_difference.unsigned_abs()))
        };
        quantize_digits(self.negative, &digits, shift, precision, scale)
    }
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
#[derive(Debug, Clone, Copy)]
enum DecimalShift {
    Append(usize),
    Discard(usize),
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
fn quantize_digits(
    negative: bool,
    significant: &[u8],
    shift: DecimalShift,
    precision: u8,
    scale: i8,
) -> Result<Decimal> {
    if significant.is_empty() {
        return Decimal::new(false, scale, 0);
    }

    let (kept, appended_zeros, round_up) = match shift {
        DecimalShift::Append(count) => (significant, count, false),
        DecimalShift::Discard(count) if count < significant.len() => {
            let kept_len = significant.len() - count;
            (
                significant.get(..kept_len).expect("bounded slice"),
                0,
                significant[kept_len] >= 5,
            )
        }
        DecimalShift::Discard(count) if count == significant.len() => {
            (&[][..], 0, significant[0] >= 5)
        }
        DecimalShift::Discard(_) => (&[][..], 0, false),
    };

    let base_digits = kept.len().saturating_add(appended_zeros);
    if base_digits > usize::from(precision) {
        return Err(precision_overflow(precision, scale));
    }

    let mut magnitude = digits_to_u128(kept).ok_or_else(|| precision_overflow(precision, scale))?;
    if appended_zeros != 0 {
        let exponent =
            u32::try_from(appended_zeros).map_err(|_| precision_overflow(precision, scale))?;
        let multiplier = 10_u128
            .checked_pow(exponent)
            .ok_or_else(|| precision_overflow(precision, scale))?;
        magnitude = magnitude
            .checked_mul(multiplier)
            .ok_or_else(|| precision_overflow(precision, scale))?;
    }
    if round_up {
        magnitude = magnitude
            .checked_add(1)
            .ok_or_else(|| precision_overflow(precision, scale))?;
    }

    let digits_after_rounding = if magnitude == 0 {
        1
    } else {
        magnitude.ilog10() as usize + 1
    };
    if magnitude != 0 && digits_after_rounding > usize::from(precision) {
        return Err(precision_overflow(precision, scale));
    }
    Decimal::new(negative, scale, magnitude).map_err(|_| precision_overflow(precision, scale))
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
fn digits_to_u128(digits: &[u8]) -> Option<u128> {
    digits.iter().try_fold(0_u128, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u128::from(*digit)))
    })
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
fn discard_shape(significant_digits: usize, discarded: usize) -> (usize, usize, Option<usize>) {
    if discarded < significant_digits {
        let kept = significant_digits - discarded;
        (kept, 0, Some(kept))
    } else if discarded == significant_digits {
        (0, 0, Some(0))
    } else {
        (0, 0, None)
    }
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
fn finish_quantized(negative: bool, magnitude: u128, precision: u8, scale: i8) -> Result<Decimal> {
    let digits_after_rounding = if magnitude == 0 {
        1
    } else {
        magnitude.ilog10() as usize + 1
    };
    if magnitude != 0 && digits_after_rounding > usize::from(precision) {
        return Err(precision_overflow(precision, scale));
    }
    Decimal::new(negative, scale, magnitude).map_err(|_| precision_overflow(precision, scale))
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
fn invalid_number(precision: u8, scale: i8) -> DbError {
    DbError::oracle(
        ErrorCode::Type,
        1722,
        format!("value is not a valid NUMBER({precision},{scale})"),
    )
}

#[allow(
    dead_code,
    reason = "Oracle normalization is wired into execution by later phase 5 tasks"
)]
fn precision_overflow(precision: u8, scale: i8) -> DbError {
    DbError::oracle(
        ErrorCode::Type,
        1438,
        format!("value exceeds precision for NUMBER({precision},{scale})"),
    )
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
