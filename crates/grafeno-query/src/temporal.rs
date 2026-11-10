//! Durations and calendar arithmetic on UTC times.
//!
//! A duration has three parts that do not convert into each other exactly:
//! months (a month has 28 to 31 days), days (a day is 24 hours, in UTC) and
//! microseconds. Adding one to a time adds the months on the calendar first
//! (the day is clamped to the end of a shorter month), then the days, then
//! the microseconds.

use grafeno::{Timestamp, days_in_month};

use crate::QueryError;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;
/// The average Gregorian month, for comparing and averaging durations.
const DAYS_PER_MONTH: f64 = 30.436_875;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Duration {
    pub months: i64,
    pub days: i64,
    pub micros: i64,
}

/// A unit letter of an ISO 8601 duration and how it adds to one.
type Unit<'a> = (char, &'a dyn Fn(&mut Duration, f64));

fn error(message: impl Into<String>) -> QueryError {
    QueryError::runtime(message)
}

impl Duration {
    pub const ZERO: Duration = Duration {
        months: 0,
        days: 0,
        micros: 0,
    };

    pub fn of_micros(micros: i64) -> Self {
        Self {
            micros,
            ..Self::ZERO
        }
    }

    /// Parses ISO 8601: `P1Y2M3W4DT5H6M7.5S`, `PT4H`, `-P1D` or `P-1D`.
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        let invalid = || {
            error(format!(
                "invalid duration {text:?}, expected ISO 8601 such as P1Y2M10D or PT4H30M"
            ))
        };
        let (negative, rest) = match text.trim().strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.trim()),
        };
        let rest = rest
            .strip_prefix(['P', 'p'])
            .ok_or_else(invalid)?
            .to_ascii_uppercase();
        let (date, time) = match rest.split_once('T') {
            Some((date, time)) => (date.to_owned(), Some(time.to_owned())),
            None => (rest.clone(), None),
        };
        let mut duration = Self::ZERO;
        let mut any = false;
        let mut read = |part: &str, units: &[Unit]| -> Result<(), QueryError> {
            let mut number = String::new();
            let mut next = 0;
            for c in part.chars() {
                if c.is_ascii_digit() || c == '.' || c == '-' || c == ',' {
                    number.push(if c == ',' { '.' } else { c });
                    continue;
                }
                let at = units[next..]
                    .iter()
                    .position(|(unit, _)| *unit == c)
                    .ok_or_else(invalid)?;
                let value: f64 = number.parse().map_err(|_| invalid())?;
                (units[next + at].1)(&mut duration, value);
                next += at + 1;
                number.clear();
                any = true;
            }
            if number.is_empty() {
                Ok(())
            } else {
                Err(invalid())
            }
        };
        let fraction_days = |d: &mut Duration, days: f64| {
            d.days += days.trunc() as i64;
            d.micros += (days.fract() * MICROS_PER_DAY as f64).round() as i64;
        };
        read(
            &date,
            &[
                ('Y', &|d, v| {
                    d.months += (v * 12.0).round() as i64;
                }),
                ('M', &|d, v| {
                    d.months += v.trunc() as i64;
                    fraction_days(d, v.fract() * DAYS_PER_MONTH);
                }),
                ('W', &|d, v| fraction_days(d, v * 7.0)),
                ('D', &|d, v| fraction_days(d, v)),
            ],
        )?;
        if let Some(time) = &time {
            if time.is_empty() {
                return Err(invalid());
            }
            read(
                time,
                &[
                    ('H', &|d, v| {
                        d.micros += (v * MICROS_PER_HOUR as f64).round() as i64
                    }),
                    ('M', &|d, v| {
                        d.micros += (v * MICROS_PER_MINUTE as f64).round() as i64
                    }),
                    ('S', &|d, v| {
                        d.micros += (v * MICROS_PER_SECOND as f64).round() as i64
                    }),
                ],
            )?;
        }
        if !any {
            return Err(invalid());
        }
        Ok(if negative {
            duration.negated()
        } else {
            duration
        })
    }

    /// ISO 8601, each part with its own sign: `P1Y2M3DT4H5M6.5S`, `PT0S`.
    pub fn iso(&self) -> String {
        let mut out = String::from("P");
        let (years, months) = (self.months / 12, self.months % 12);
        if years != 0 {
            out.push_str(&format!("{years}Y"));
        }
        if months != 0 {
            out.push_str(&format!("{months}M"));
        }
        if self.days != 0 {
            out.push_str(&format!("{}D", self.days));
        }
        if self.micros != 0 || out == "P" {
            out.push('T');
            let micros = self.micros;
            let hours = micros / MICROS_PER_HOUR;
            let minutes = micros % MICROS_PER_HOUR / MICROS_PER_MINUTE;
            let rest = micros % MICROS_PER_MINUTE;
            if hours != 0 {
                out.push_str(&format!("{hours}H"));
            }
            if minutes != 0 {
                out.push_str(&format!("{minutes}M"));
            }
            if rest != 0 || (hours == 0 && minutes == 0) {
                let seconds = rest / MICROS_PER_SECOND;
                let fraction = (rest % MICROS_PER_SECOND).abs();
                if fraction == 0 {
                    out.push_str(&format!("{seconds}S"));
                } else {
                    let sign = if rest < 0 && seconds == 0 { "-" } else { "" };
                    let digits = format!("{fraction:06}");
                    out.push_str(&format!(
                        "{sign}{seconds}.{}S",
                        digits.trim_end_matches('0')
                    ));
                }
            }
        }
        out
    }

    pub fn negated(self) -> Self {
        Self {
            months: -self.months,
            days: -self.days,
            micros: -self.micros,
        }
    }

    pub fn plus(self, other: Self) -> Result<Self, QueryError> {
        let overflow = || error("duration overflow");
        Ok(Self {
            months: self.months.checked_add(other.months).ok_or_else(overflow)?,
            days: self.days.checked_add(other.days).ok_or_else(overflow)?,
            micros: self.micros.checked_add(other.micros).ok_or_else(overflow)?,
        })
    }

    /// Multiplied by a number; fractions of months and days spill into the
    /// smaller parts.
    pub fn scale(self, factor: f64) -> Result<Self, QueryError> {
        if !factor.is_finite() {
            return Err(error("cannot scale a duration by a non-finite number"));
        }
        let months = self.months as f64 * factor;
        let days = self.days as f64 * factor + months.fract() * DAYS_PER_MONTH;
        let micros = self.micros as f64 * factor + days.fract() * MICROS_PER_DAY as f64;
        if months.abs() > 1e15 || days.abs() > 1e15 || micros.abs() > 9e18 {
            return Err(error("duration overflow"));
        }
        Ok(Self {
            months: months.trunc() as i64,
            days: days.trunc() as i64,
            micros: micros.round() as i64,
        })
    }

    /// Approximate length in microseconds (a month is 30.44 days), used to
    /// compare, sort and average durations.
    pub fn approx_micros(&self) -> f64 {
        (self.months as f64 * DAYS_PER_MONTH + self.days as f64) * MICROS_PER_DAY as f64
            + self.micros as f64
    }

    /// A component: `years`, `quarters` and `months` read the
    /// months; `weeks` and `days` the days; `hours` … `microseconds` the
    /// time part; `*OfYear`-style names the remainder within the larger
    /// unit.
    pub fn component(&self, name: &str) -> Option<i64> {
        let micros = self.micros;
        Some(match name {
            "years" => self.months / 12,
            "quarters" => self.months / 3,
            "months" => self.months,
            "monthsOfYear" => self.months % 12,
            "monthsOfQuarter" => self.months % 3,
            "quartersOfYear" => self.months / 3 % 4,
            "weeks" => self.days / 7,
            "days" => self.days,
            "daysOfWeek" => self.days % 7,
            "hours" => micros / MICROS_PER_HOUR,
            "minutes" => micros / MICROS_PER_MINUTE,
            "minutesOfHour" => micros / MICROS_PER_MINUTE % 60,
            "seconds" => micros / MICROS_PER_SECOND,
            "secondsOfMinute" => micros / MICROS_PER_SECOND % 60,
            "milliseconds" => micros / 1_000,
            "millisecondsOfSecond" => micros / 1_000 % 1_000,
            "microseconds" => micros,
            "microsecondsOfSecond" => micros % MICROS_PER_SECOND,
            _ => return None,
        })
    }
}

pub const DURATION_COMPONENTS: &str = "years, quarters, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, monthsOfYear, daysOfWeek, minutesOfHour, secondsOfMinute, millisecondsOfSecond, microsecondsOfSecond";

/// Adds months on the calendar, clamping the day to the month's end.
fn add_months(time: Timestamp, months: i64) -> Result<Timestamp, QueryError> {
    if months == 0 {
        return Ok(time);
    }
    let (year, month, day) = time.civil();
    let total = year * 12 + (month - 1) + months;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    let day = day.min(days_in_month(year, month));
    let midnight =
        Timestamp::from_civil(year, month, day).ok_or_else(|| error("date out of range"))?;
    Ok(Timestamp::from_micros(
        midnight.as_micros() + time.micros_of_day(),
    ))
}

/// `time + duration`: months on the calendar, then days, then microseconds.
pub fn add(time: Timestamp, duration: Duration) -> Result<Timestamp, QueryError> {
    let time = add_months(time, duration.months)?;
    let micros = duration
        .days
        .checked_mul(MICROS_PER_DAY)
        .and_then(|days| days.checked_add(duration.micros))
        .and_then(|delta| time.as_micros().checked_add(delta))
        .ok_or_else(|| error("time out of range"))?;
    Ok(Timestamp::from_micros(micros))
}

/// The calendar duration from `a` to `b`: whole months, then whole days,
/// then the rest (negative when `b` is before `a`).
pub fn between(a: Timestamp, b: Timestamp) -> Result<Duration, QueryError> {
    if b < a {
        return Ok(between(b, a)?.negated());
    }
    let (ya, ma, _) = a.civil();
    let (yb, mb, _) = b.civil();
    let mut months = (yb * 12 + mb) - (ya * 12 + ma);
    while months > 0 && add_months(a, months)? > b {
        months -= 1;
    }
    let after_months = add_months(a, months)?;
    let rest = b.as_micros() - after_months.as_micros();
    Ok(Duration {
        months,
        days: rest / MICROS_PER_DAY,
        micros: rest % MICROS_PER_DAY,
    })
}

/// `duration.inMonths`, `inDays`, `inSeconds`: the duration in one unit.
pub fn between_in(unit: &str, a: Timestamp, b: Timestamp) -> Result<Duration, QueryError> {
    let micros = b.as_micros() - a.as_micros();
    Ok(match unit {
        "months" => Duration {
            months: between(a, b)?.months,
            ..Duration::ZERO
        },
        "days" => Duration {
            days: micros / MICROS_PER_DAY,
            ..Duration::ZERO
        },
        _ => Duration::of_micros(micros),
    })
}

/// ISO week date: (week-based year, week 1 to 53).
fn iso_week(time: Timestamp) -> (i64, i64) {
    let (year, _, _) = time.civil();
    // Monday = 1 … Sunday = 7; 1970-01-01 was a Thursday.
    let weekday = |days: i64| (days + 3).rem_euclid(7) + 1;
    let days = time.days();
    let start_of = |year: i64| {
        let jan4 = Timestamp::from_civil(year, 1, 4)
            .expect("valid date")
            .days();
        jan4 - (weekday(jan4) - 1)
    };
    let mut week_year = year;
    if days < start_of(year) {
        week_year -= 1;
    } else if days >= start_of(year + 1) {
        week_year += 1;
    }
    (week_year, (days - start_of(week_year)) / 7 + 1)
}

/// A part of a time: `year`, `quarter`, `month`, `week`, `weekYear`, `day`,
/// `dayOfWeek` (Monday is 1), `dayOfYear`, `hour`, `minute`, `second`,
/// `millisecond`, `microsecond`, `epochSeconds`, `epochMillis`.
pub fn part(time: Timestamp, name: &str) -> Option<i64> {
    let (year, month, day) = time.civil();
    let of_day = time.micros_of_day();
    Some(match name {
        "year" => year,
        "quarter" => (month - 1) / 3 + 1,
        "month" => month,
        "week" => iso_week(time).1,
        "weekYear" => iso_week(time).0,
        "day" => day,
        "dayOfWeek" | "weekDay" => (time.days() + 3).rem_euclid(7) + 1,
        "dayOfYear" | "ordinalDay" => {
            time.days()
                - Timestamp::from_civil(year, 1, 1)
                    .expect("valid date")
                    .days()
                + 1
        }
        "hour" => of_day / MICROS_PER_HOUR,
        "minute" => of_day / MICROS_PER_MINUTE % 60,
        "second" => of_day / MICROS_PER_SECOND % 60,
        "millisecond" => of_day / 1_000 % 1_000,
        "microsecond" => of_day % MICROS_PER_SECOND,
        "epochSeconds" => time.as_micros().div_euclid(MICROS_PER_SECOND),
        "epochMillis" => time.as_micros().div_euclid(1_000),
        _ => return None,
    })
}

pub const TIME_PARTS: &str = "year, quarter, month, week, weekYear, day, dayOfWeek, dayOfYear, hour, minute, second, millisecond, microsecond, epochSeconds, epochMillis";

/// The start of the unit that contains `time`: `millennium`, `century`,
/// `decade`, `year`, `quarter`, `month`, `week` (Monday), `day`, `hour`,
/// `minute`, `second`.
pub fn truncate(unit: &str, time: Timestamp) -> Result<Timestamp, QueryError> {
    let (year, month, _) = time.civil();
    let date = |year: i64, month: i64, day: i64| {
        Timestamp::from_civil(year, month, day).ok_or_else(|| error("date out of range"))
    };
    let floor = |step: i64| Timestamp::from_micros(time.as_micros().div_euclid(step) * step);
    match unit.to_ascii_lowercase().as_str() {
        "millennium" => date(year.div_euclid(1000) * 1000, 1, 1),
        "century" => date(year.div_euclid(100) * 100, 1, 1),
        "decade" => date(year.div_euclid(10) * 10, 1, 1),
        "year" => date(year, 1, 1),
        "quarter" => date(year, (month - 1) / 3 * 3 + 1, 1),
        "month" => date(year, month, 1),
        "week" => {
            let weekday = (time.days() + 3).rem_euclid(7);
            Ok(Timestamp::from_micros(
                (time.days() - weekday) * MICROS_PER_DAY,
            ))
        }
        "day" => Ok(floor(MICROS_PER_DAY)),
        "hour" => Ok(floor(MICROS_PER_HOUR)),
        "minute" => Ok(floor(MICROS_PER_MINUTE)),
        "second" => Ok(floor(MICROS_PER_SECOND)),
        other => Err(error(format!(
            "cannot truncate to {other:?}: use millennium, century, decade, year, quarter, month, week, day, hour, minute or second"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> Timestamp {
        Timestamp::parse(text).unwrap()
    }

    #[test]
    fn parses_and_prints_iso_durations() {
        let d = Duration::parse("P1Y2M3DT4H5M6.5S").unwrap();
        assert_eq!((d.months, d.days), (14, 3));
        assert_eq!(
            d.micros,
            4 * MICROS_PER_HOUR + 5 * MICROS_PER_MINUTE + 6_500_000
        );
        assert_eq!(d.iso(), "P1Y2M3DT4H5M6.5S");
        assert_eq!(Duration::parse("PT90M").unwrap().iso(), "PT1H30M");
        assert_eq!(Duration::parse("P2W").unwrap().iso(), "P14D");
        assert_eq!(Duration::parse("-P1D").unwrap().iso(), "P-1D");
        assert_eq!(Duration::parse("P0D").unwrap().iso(), "PT0S");
        assert_eq!(Duration::parse("PT0.25S").unwrap().iso(), "PT0.25S");
        assert_eq!(Duration::parse("P1.5D").unwrap().iso(), "P1DT12H");
        for bad in ["", "P", "1D", "PT", "P1H", "PXD", "P1D2"] {
            assert!(Duration::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn calendar_arithmetic() {
        let month = Duration::parse("P1M").unwrap();
        assert_eq!(add(t("2024-01-31"), month).unwrap(), t("2024-02-29"));
        assert_eq!(
            add(t("2023-01-31T10:00:00Z"), month).unwrap(),
            t("2023-02-28T10:00:00Z")
        );
        assert_eq!(
            add(t("2024-03-01"), month.negated()).unwrap(),
            t("2024-02-01")
        );
        assert_eq!(
            add(t("2024-12-31T23:00:00Z"), Duration::parse("PT2H").unwrap()).unwrap(),
            t("2025-01-01T01:00:00Z")
        );

        let span = between(t("2023-01-15"), t("2024-03-20T06:00:00Z")).unwrap();
        assert_eq!(span.iso(), "P1Y2M5DT6H");
        assert_eq!(
            between(t("2024-03-20"), t("2024-01-15")).unwrap().iso(),
            "P-2M-5D"
        );
        assert_eq!(
            between(t("2024-01-31"), t("2024-02-29")).unwrap().iso(),
            "P1M"
        );
        assert_eq!(
            between_in("days", t("2024-01-01"), t("2024-03-01"))
                .unwrap()
                .days,
            60
        );
        assert_eq!(
            between_in("months", t("2024-01-01"), t("2024-03-01"))
                .unwrap()
                .months,
            2
        );
    }

    #[test]
    fn parts_and_truncation() {
        let time = t("2024-12-30T13:45:30.250Z");
        assert_eq!(part(time, "year"), Some(2024));
        assert_eq!(part(time, "quarter"), Some(4));
        assert_eq!(part(time, "dayOfWeek"), Some(1), "a Monday");
        assert_eq!(
            (part(time, "weekYear"), part(time, "week")),
            (Some(2025), Some(1))
        );
        assert_eq!(part(t("2021-01-03"), "week"), Some(53));
        assert_eq!(part(time, "dayOfYear"), Some(365));
        assert_eq!(part(time, "millisecond"), Some(250));
        assert_eq!(truncate("month", time).unwrap(), t("2024-12-01"));
        assert_eq!(
            truncate("quarter", t("2024-08-15")).unwrap(),
            t("2024-07-01")
        );
        assert_eq!(truncate("week", t("2024-08-15")).unwrap(), t("2024-08-12"));
        assert_eq!(truncate("hour", time).unwrap(), t("2024-12-30T13:00:00Z"));
        assert!(truncate("fortnight", time).is_err());
    }
}
