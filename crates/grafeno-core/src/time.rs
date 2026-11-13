use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// An instant in microseconds since the Unix epoch (UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const MIN: Self = Self(i64::MIN);
    pub const MAX: Self = Self(i64::MAX);

    pub fn now() -> Self {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(elapsed.as_micros() as i64)
    }

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    pub const fn from_secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1_000_000))
    }

    pub const fn as_micros(self) -> i64 {
        self.0
    }

    /// Parses an ISO 8601 date or date-time: `2024-05-01`,
    /// `2024-05-01T10:30`, `2024-05-01T10:30:00.250Z` or
    /// `2024-05-01T10:30:00-05:00`. Without an offset, UTC is assumed.
    pub fn parse(text: &str) -> Result<Self> {
        let invalid = || Error::Invalid(format!("invalid date {text:?}, expected ISO 8601"));
        let number = |part: &str| -> Result<i64> {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            part.parse().map_err(|_| invalid())
        };
        let (date, time) = match text.split_once(['T', ' ']) {
            Some((date, time)) => (date, Some(time)),
            None => (text, None),
        };
        let mut parts = date.splitn(3, '-');
        let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        let (year, month, day) = (number(year)?, number(month)?, number(day)?);
        // Microseconds since 1970 hold about 292 000 years either way: a
        // later year is refused, not wrapped around (or, in a debug build,
        // a panic).
        if year > MAX_YEAR
            || !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
        {
            return Err(invalid());
        }
        let mut micros = days_from_civil(year, month, day)
            .checked_mul(86_400_000_000)
            .ok_or_else(invalid)?;

        if let Some(time) = time {
            let (clock, offset) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
                (clock, 0)
            } else if let Some(at) = time.rfind(['+', '-']) {
                let (clock, offset) = time.split_at(at);
                let sign = if offset.starts_with('-') { -1 } else { 1 };
                let (hours, minutes) = offset[1..].split_once(':').ok_or_else(invalid)?;
                let (hours, minutes) = (number(hours)?, number(minutes)?);
                if hours > 23 || minutes > 59 {
                    return Err(invalid());
                }
                (clock, sign * (hours * 60 + minutes) * 60_000_000)
            } else {
                (time, 0)
            };
            let (clock, fraction) = match clock.split_once('.') {
                Some((clock, fraction)) => (clock, Some(fraction)),
                None => (clock, None),
            };
            let mut fields = clock.split(':');
            let hours = number(fields.next().ok_or_else(invalid)?)?;
            let minutes = number(fields.next().ok_or_else(invalid)?)?;
            let seconds = fields.next().map(number).transpose()?.unwrap_or(0);
            if fields.next().is_some() || hours > 23 || minutes > 59 || seconds > 59 {
                return Err(invalid());
            }
            micros = micros
                .checked_add(((hours * 60 + minutes) * 60 + seconds) * 1_000_000 - offset)
                .ok_or_else(invalid)?;
            if let Some(fraction) = fraction {
                number(fraction)?;
                let digits: String = fraction.chars().chain("000000".chars()).take(6).collect();
                micros = micros.checked_add(number(&digits)?).ok_or_else(invalid)?;
            }
        }
        Ok(Self(micros))
    }

    /// The UTC calendar date as `YYYY-MM-DD` (proleptic Gregorian).
    pub fn date(self) -> String {
        let (year, month, day) = self.civil();
        format!("{year:04}-{month:02}-{day:02}")
    }

    /// The UTC calendar date: year, month (1 to 12) and day (1 to 31).
    pub fn civil(self) -> (i64, i64, i64) {
        civil_from_days(self.days())
    }

    /// Whole days since 1970-01-01 (negative before it).
    pub fn days(self) -> i64 {
        self.0.div_euclid(86_400_000_000)
    }

    /// Microseconds since midnight UTC.
    pub fn micros_of_day(self) -> i64 {
        self.0.rem_euclid(86_400_000_000)
    }

    /// Midnight UTC of a calendar date, or `None` if there is no such date.
    pub fn from_civil(year: i64, month: i64, day: i64) -> Option<Self> {
        ((1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month))
            .then(|| Self(days_from_civil(year, month, day) * 86_400_000_000))
    }

    /// ISO 8601 in UTC: `2024-05-01T10:30:00Z`, with microseconds only when
    /// they are not zero (`…:00.250000Z`). [`Timestamp::parse`] reads it back.
    pub fn iso(self) -> String {
        let micros_of_day = self.0.rem_euclid(86_400_000_000);
        let seconds = micros_of_day / 1_000_000;
        let fraction = micros_of_day % 1_000_000;
        let clock = format!(
            "{:02}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        );
        if fraction == 0 {
            format!("{}T{clock}Z", self.date())
        } else {
            format!("{}T{clock}.{fraction:06}Z", self.date())
        }
    }
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
/// The latest year a date may have.
pub const MAX_YEAR: i64 = 200_000;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days in a month of the proleptic Gregorian calendar.
pub fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_dates() {
        fn parse(text: &str) -> Timestamp {
            Timestamp::parse(text).unwrap()
        }
        assert_eq!(parse("1970-01-01"), Timestamp::from_secs(0));
        // Found by an outside review: a far year overflowed the microseconds
        // (wrapped around in release, a panic in debug).
        for far in ["999999-01-01", "200001-01-01", "9223372036854775807-01-01"] {
            assert!(Timestamp::parse(far).is_err(), "{far}");
        }
        let latest = Timestamp::parse("200000-12-31T23:59:59.999999Z").unwrap();
        assert!(latest > parse("9999-12-31"));
        assert_eq!(parse("2026-11-03"), Timestamp::from_secs(1_793_664_000));
        assert_eq!(
            parse("2026-11-03T01:02:03Z"),
            Timestamp::from_secs(1_793_667_723)
        );
        assert_eq!(
            parse("2026-11-03T01:02"),
            Timestamp::from_secs(1_793_667_720)
        );
        assert_eq!(
            parse("2026-11-03T01:02:03-05:00"),
            Timestamp::from_secs(1_793_685_723)
        );
        assert_eq!(
            parse("1970-01-01T00:00:00.25Z"),
            Timestamp::from_micros(250_000)
        );
        for bad in [
            "",
            "2026-13-01",
            "2026-02-29",
            "2026-11-03T25:00",
            "yesterday",
            "2026-11",
        ] {
            assert!(Timestamp::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse(&parse("2000-02-29").date()), parse("2000-02-29"));
        for text in [
            "2026-11-03T01:02:03Z",
            "1969-12-31T23:59:59.999999Z",
            "2000-02-29T00:00:00Z",
        ] {
            assert_eq!(parse(text).iso(), text);
        }
    }

    #[test]
    fn formats_dates() {
        assert_eq!(Timestamp::from_secs(0).date(), "1970-01-01");
        assert_eq!(Timestamp::from_secs(951_782_400).date(), "2000-02-29");
        assert_eq!(Timestamp::from_secs(1_793_664_000).date(), "2026-11-03");
        assert_eq!(Timestamp::from_secs(-1).date(), "1969-12-31");
    }
}
