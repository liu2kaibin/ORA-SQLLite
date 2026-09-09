use crate::{DbError, ErrorCode, Result};

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

    pub const fn year(self) -> u16 {
        self.year
    }

    pub const fn month(self) -> u8 {
        self.month
    }

    pub const fn day(self) -> u8 {
        self.day
    }

    pub const fn hour(self) -> u8 {
        self.hour
    }

    pub const fn minute(self) -> u8 {
        self.minute
    }

    pub const fn second(self) -> u8 {
        self.second
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleTimestamp {
    date: OracleDate,
    micros: u32,
    offset_minutes: Option<i16>,
}

impl OracleTimestamp {
    pub fn new(date: OracleDate, micros: u32, offset_minutes: Option<i16>) -> Result<Self> {
        if micros > 999_999 {
            return Err(type_error("microseconds must be between 0 and 999999"));
        }

        if offset_minutes.is_some_and(|offset| !(-840..=840).contains(&offset)) {
            return Err(type_error("offset minutes must be between -840 and 840"));
        }

        Ok(Self {
            date,
            micros,
            offset_minutes,
        })
    }

    pub const fn date(self) -> OracleDate {
        self.date
    }

    pub const fn micros(self) -> u32 {
        self.micros
    }

    pub const fn offset_minutes(self) -> Option<i16> {
        self.offset_minutes
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
