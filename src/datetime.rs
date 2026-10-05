use crate::{DbError, ErrorCode, Result};

/// An Oracle-compatible proleptic Gregorian date and whole-second time.
///
/// Years are limited to `1..=9999`; leap years use Gregorian rules. This value has no time-zone
/// offset and no fractional-second component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleDate {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl OracleDate {
    /// Constructs a checked date and time.
    ///
    /// Valid fields are year `1..=9999`, month `1..=12`, a day valid for that month, hour
    /// `0..=23`, and minute and second `0..=59`. Invalid input returns [`ErrorCode::Type`].
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Result<Self> {
        if !(1..=9999).contains(&year) {
            return Err(type_error("year must be between 1 and 9999"));
        }

        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => return Err(type_error("month must be between 1 and 12")),
        };

        if day == 0 || day > days_in_month {
            return Err(type_error("day is invalid for the specified month"));
        }

        if hour > 23 {
            return Err(type_error("hour must be between 0 and 23"));
        }

        if minute > 59 {
            return Err(type_error("minute must be between 0 and 59"));
        }

        if second > 59 {
            return Err(type_error("second must be between 0 and 59"));
        }

        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    /// Returns the year in `1..=9999`.
    pub const fn year(self) -> u16 {
        self.year
    }

    /// Returns the month in `1..=12`.
    pub const fn month(self) -> u8 {
        self.month
    }

    /// Returns the one-based day of the month.
    pub const fn day(self) -> u8 {
        self.day
    }

    /// Returns the hour in `0..=23`.
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// Returns the minute in `0..=59`.
    pub const fn minute(self) -> u8 {
        self.minute
    }

    /// Returns the second in `0..=59`.
    pub const fn second(self) -> u8 {
        self.second
    }
}

/// An Oracle-compatible timestamp with nanosecond precision and an optional fixed offset.
///
/// The fractional component is nanoseconds within the second. An offset, when present, is a fixed
/// number of minutes in `-840..=840`; no daylight-saving or named-zone rules are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleTimestamp {
    date: OracleDate,
    nanos: u32,
    offset_minutes: Option<i16>,
}

impl OracleTimestamp {
    /// Constructs a checked timestamp from microseconds within the second.
    ///
    /// `micros` must be in `0..1_000_000` and `offset_minutes`, when present, must be in
    /// `-840..=840`. Invalid input returns [`ErrorCode::Type`].
    pub fn new(date: OracleDate, micros: u32, offset_minutes: Option<i16>) -> Result<Self> {
        let nanos = micros
            .checked_mul(1_000)
            .ok_or_else(|| type_error("microseconds exceed timestamp range"))?;
        Self::from_nanoseconds(date, nanos, offset_minutes)
    }

    /// Constructs a checked timestamp from nanoseconds within the second.
    ///
    /// `nanos` must be in `0..1_000_000_000` and `offset_minutes`, when present, must be in
    /// `-840..=840`. Invalid input returns [`ErrorCode::Type`].
    pub fn from_nanoseconds(
        date: OracleDate,
        nanos: u32,
        offset_minutes: Option<i16>,
    ) -> Result<Self> {
        if nanos >= 1_000_000_000 {
            return Err(type_error("nanoseconds must be below one second"));
        }
        if offset_minutes.is_some_and(|offset| !(-840..=840).contains(&offset)) {
            return Err(type_error("offset minutes must be between -840 and 840"));
        }

        Ok(Self {
            date,
            nanos,
            offset_minutes,
        })
    }

    /// Returns the calendar date and whole-second time.
    pub const fn date(self) -> OracleDate {
        self.date
    }

    /// Returns the fractional second truncated to microseconds.
    pub const fn micros(self) -> u32 {
        self.nanos / 1_000
    }

    /// Returns nanoseconds within the second, in `0..1_000_000_000`.
    pub const fn nanoseconds(self) -> u32 {
        self.nanos
    }

    /// Returns the optional fixed offset in minutes.
    pub const fn offset_minutes(self) -> Option<i16> {
        self.offset_minutes
    }

    #[allow(
        dead_code,
        reason = "consumed by later Phase 5 timestamp comparison and index tasks"
    )]
    pub(crate) fn utc_day_and_nanos(self) -> Result<(i32, u64)> {
        const NANOS_PER_SECOND: i64 = 1_000_000_000;
        const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SECOND;
        const NANOS_PER_DAY: i64 = 24 * 60 * NANOS_PER_MINUTE;

        let date = self.date;
        let day =
            i64::from(days_before_year(date.year()) + days_before_month(date.year(), date.month()))
                + i64::from(date.day())
                - 1;
        let second_of_day = i64::from(date.hour()) * 3_600
            + i64::from(date.minute()) * 60
            + i64::from(date.second());
        let local_nanos = second_of_day * NANOS_PER_SECOND + i64::from(self.nanos);
        let utc_nanos =
            local_nanos - i64::from(self.offset_minutes.unwrap_or(0)) * NANOS_PER_MINUTE;
        let utc_day = day + utc_nanos.div_euclid(NANOS_PER_DAY);
        let nanos_of_day = utc_nanos.rem_euclid(NANOS_PER_DAY);
        Ok((
            i32::try_from(utc_day).map_err(|_| type_error("UTC day exceeds timestamp range"))?,
            u64::try_from(nanos_of_day).map_err(|_| type_error("UTC nanoseconds are invalid"))?,
        ))
    }
}

#[allow(
    dead_code,
    reason = "consumed by utc_day_and_nanos in later Phase 5 tasks"
)]
const fn days_before_year(year: u16) -> u32 {
    let prior = year as u32 - 1;
    prior * 365 + prior / 4 - prior / 100 + prior / 400
}

#[allow(
    dead_code,
    reason = "consumed by utc_day_and_nanos in later Phase 5 tasks"
)]
const fn days_before_month(year: u16, month: u8) -> u32 {
    let before = match month {
        1 => 0,
        2 => 31,
        3 => 59,
        4 => 90,
        5 => 120,
        6 => 151,
        7 => 181,
        8 => 212,
        9 => 243,
        10 => 273,
        11 => 304,
        12 => 334,
        _ => 0,
    };
    before
        + if month > 2 && is_leap_year(year) {
            1
        } else {
            0
        }
}

const fn is_leap_year(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn type_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message)
}

#[cfg(test)]
mod tests {
    use super::{OracleDate, OracleTimestamp};
    use crate::ErrorCode;

    #[test]
    fn validates_calendar_and_time_fields() {
        assert!(OracleDate::new(2024, 2, 29, 23, 59, 59).is_ok());
        assert!(OracleDate::new(2023, 2, 29, 0, 0, 0).is_err());
        assert!(OracleDate::new(2026, 1, 1, 24, 0, 0).is_err());
    }

    #[test]
    fn validates_fraction_and_fixed_offset() {
        let date = OracleDate::new(2026, 9, 1, 12, 0, 0).expect("valid date");
        assert!(OracleTimestamp::new(date, 999_999, Some(840)).is_ok());
        assert!(OracleTimestamp::new(date, 1_000_000, None).is_err());
        assert!(OracleTimestamp::new(date, 0, Some(841)).is_err());
    }

    #[test]
    fn nanoseconds_preserve_the_old_microsecond_api_and_offset_limits() {
        let date = OracleDate::new(2026, 1, 2, 3, 4, 5).unwrap();
        let old = OracleTimestamp::new(date, 123_456, Some(480)).unwrap();
        assert_eq!(old.nanoseconds(), 123_456_000);
        assert_eq!(old.micros(), 123_456);

        let precise = OracleTimestamp::from_nanoseconds(date, 123_456_789, None).unwrap();
        assert_eq!(precise.nanoseconds(), 123_456_789);
        assert_eq!(precise.micros(), 123_456);
        assert!(OracleTimestamp::from_nanoseconds(date, 999_999_999, None).is_ok());
        assert!(OracleTimestamp::from_nanoseconds(date, 1_000_000_000, None).is_err());
        assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(840)).is_ok());
        assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(-840)).is_ok());
        assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(841)).is_err());
        assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(-841)).is_err());
    }

    #[test]
    fn utc_day_and_nanos_preserves_instant_across_calendar_boundaries() {
        let local = OracleTimestamp::from_nanoseconds(
            OracleDate::new(2026, 1, 1, 0, 30, 0).unwrap(),
            123_456_789,
            Some(60),
        )
        .unwrap();
        let utc = OracleTimestamp::from_nanoseconds(
            OracleDate::new(2025, 12, 31, 23, 30, 0).unwrap(),
            123_456_789,
            Some(0),
        )
        .unwrap();
        assert_eq!(
            local.utc_day_and_nanos().unwrap(),
            utc.utc_day_and_nanos().unwrap()
        );

        let earliest = OracleTimestamp::from_nanoseconds(
            OracleDate::new(1, 1, 1, 0, 0, 0).unwrap(),
            0,
            Some(840),
        )
        .unwrap();
        assert_eq!(
            earliest.utc_day_and_nanos().unwrap(),
            (-1, 36_000_000_000_000)
        );
    }

    #[test]
    fn validates_date_field_boundaries() {
        assert!(OracleDate::new(1, 1, 1, 0, 0, 0).is_ok());
        assert!(OracleDate::new(9999, 12, 31, 23, 59, 59).is_ok());
        assert!(OracleDate::new(0, 1, 1, 0, 0, 0).is_err());
        assert!(OracleDate::new(10_000, 1, 1, 0, 0, 0).is_err());
        assert!(OracleDate::new(2024, 0, 1, 0, 0, 0).is_err());
        assert!(OracleDate::new(2024, 13, 1, 0, 0, 0).is_err());
        assert!(OracleDate::new(2024, 1, 0, 0, 0, 0).is_err());
        assert!(OracleDate::new(2024, 4, 31, 0, 0, 0).is_err());
        assert!(OracleDate::new(1900, 2, 29, 0, 0, 0).is_err());
        assert!(OracleDate::new(2000, 2, 29, 0, 0, 0).is_ok());
        assert!(OracleDate::new(2024, 1, 1, 23, 60, 0).is_err());
        let error = OracleDate::new(2024, 1, 1, 23, 59, 60).expect_err("invalid second");

        assert_eq!(error.code(), ErrorCode::Type);
    }

    #[test]
    fn validates_negative_offset_boundary() {
        let date = OracleDate::new(2026, 9, 1, 12, 0, 0).expect("valid date");
        assert!(OracleTimestamp::new(date, 0, Some(-840)).is_ok());
        let error = OracleTimestamp::new(date, 0, Some(-841)).expect_err("invalid offset");

        assert_eq!(error.code(), ErrorCode::Type);
    }

    #[test]
    fn stores_all_date_and_timestamp_fields() {
        let date = OracleDate::new(2026, 9, 1, 12, 34, 56).expect("valid date");
        let other_year = OracleDate::new(2027, 9, 1, 12, 34, 56).expect("valid date");
        let other_month = OracleDate::new(2026, 10, 1, 12, 34, 56).expect("valid date");
        let other_day = OracleDate::new(2026, 9, 2, 12, 34, 56).expect("valid date");
        let other_hour = OracleDate::new(2026, 9, 1, 13, 34, 56).expect("valid date");
        let other_minute = OracleDate::new(2026, 9, 1, 12, 35, 56).expect("valid date");
        let other_second = OracleDate::new(2026, 9, 1, 12, 34, 57).expect("valid date");
        let timestamp = OracleTimestamp::new(date, 123_456, Some(480)).expect("valid timestamp");

        assert_ne!(date, other_year);
        assert_ne!(date, other_month);
        assert_ne!(date, other_day);
        assert_ne!(date, other_hour);
        assert_ne!(date, other_minute);
        assert_ne!(date, other_second);
        assert_ne!(
            timestamp,
            OracleTimestamp::new(other_day, 123_456, Some(480)).expect("valid timestamp")
        );
        assert_ne!(
            timestamp,
            OracleTimestamp::new(date, 123_457, Some(480)).expect("valid timestamp")
        );
        assert_ne!(
            timestamp,
            OracleTimestamp::new(date, 123_456, None).expect("valid timestamp")
        );
    }
}
