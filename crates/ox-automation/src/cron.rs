//! Five-field cron evaluation with DST-aware next-occurrence calculation.
//!
//! Fields are compiled into bitmaps once; a tick only walks candidate civil
//! minutes in the requested zone. A nonexistent civil time in a spring-forward
//! gap resolves to the time after the gap; a repeated civil time in a fall-back
//! resolves to its first occurrence, so a daily job never fires twice.

use anyhow::{Result, ensure};
use jiff::civil;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const HORIZON_MS: i64 = 366 * 5 * DAY_MS;

/// A compiled five-field expression: minute, hour, day-of-month, month,
/// day-of-week. `dom_any`/`dow_any` implement cron's OR rule: when both day
/// fields are restricted either may match, otherwise only the restricted one
/// counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    dom_any: bool,
    dow_any: bool,
}

impl std::str::FromStr for Cron {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self> {
        Self::parse(input)
    }
}

impl Cron {
    /// Compile an expression. Named months and weekdays are accepted apart
    /// from numbers; unknown syntax fails here rather than at every tick.
    pub fn parse(input: &str) -> Result<Self> {
        let fields: Vec<&str> = input.split_whitespace().collect();
        ensure!(
            fields.len() == 5,
            "cron needs exactly 5 fields, got {}",
            fields.len()
        );
        let minutes = parse_field(fields[0], 0, 59, None)?;
        let hours = parse_field(fields[1], 0, 23, None)?;
        let (days, dom_any) = parse_field(fields[2], 1, 31, None)?;
        let months = parse_field(fields[3], 1, 12, Some(&MONTHS))?;
        let (weekdays, dow_any) = parse_field(fields[4], 0, 7, Some(&WEEKDAYS))?;
        Ok(Self {
            minutes: minutes.0,
            hours: u32::try_from(hours.0)?,
            days: u32::try_from(days)?,
            months: u16::try_from(months.0)?,
            weekdays: u8::try_from(weekdays & 0x7f)?,
            dom_any,
            dow_any,
        })
    }

    /// First instant strictly after `now_ms` whose local civil time matches.
    /// `None` means there is no occurrence within a five-year horizon (for
    /// example `0 0 30 2 *`); it is never an excuse to fire at the wrong time.
    pub fn next_after(&self, now_ms: i64, tz: &str) -> Result<Option<i64>> {
        let zone = TimeZone::get(tz)?;
        let now = Timestamp::from_millisecond(now_ms)?;
        let horizon = now_ms.saturating_add(HORIZON_MS);
        let local = now.to_zoned_datetime(zone.clone()).datetime();
        let limit = civil::date(local.year().saturating_add(5), 1, 1);
        // Start on the current civil minute: its instant may still be in the
        // future during a fold, and the strict `>` check below rejects it.
        let mut candidate = civil::date(local.year(), local.month(), local.day()).at(
            local.hour(),
            local.minute(),
            0,
            0,
        );

        while candidate.date() < limit {
            let date = candidate.date();
            let month = u8::try_from(date.month())?;
            let hour = u8::try_from(candidate.hour())?;
            let minute = u8::try_from(candidate.minute())?;

            if self.months & (1 << month) != 0
                && self.day_matches(date)
                && self.hours & (1 << hour) != 0
                && self.minutes & (1 << minute) != 0
            {
                let instant = to_timestamp(candidate, &zone)?;
                if instant > now && instant.as_millisecond() <= horizon {
                    return Ok(Some(instant.as_millisecond()));
                }
            }

            candidate = if self.months & (1 << month) == 0 {
                let (year, next_month) = if month == 12 {
                    (date.year() + 1, 1)
                } else {
                    (date.year(), month + 1)
                };
                civil::date(year, i8::try_from(next_month)?, 1).at(0, 0, 0, 0)
            } else if !self.day_matches(date) {
                date.tomorrow()?.at(0, 0, 0, 0)
            } else if self.hours & (1 << hour) == 0 {
                if hour == 23 {
                    date.tomorrow()?.at(0, 0, 0, 0)
                } else {
                    date.at(i8::try_from(hour + 1)?, 0, 0, 0)
                }
            } else {
                candidate.checked_add(Span::new().minutes(1))?
            };
        }
        Ok(None)
    }

    fn day_matches(&self, date: civil::Date) -> bool {
        let day = u8::try_from(date.day()).expect("day is 1..=31");
        let dow = date.weekday().to_sunday_zero_offset() as u8;
        let dom = self.days & (1 << day) != 0;
        let weekday = self.weekdays & (1 << dow) != 0;
        match (self.dom_any, self.dow_any) {
            (false, false) => dom || weekday,
            (false, true) => dom,
            (true, false) => weekday,
            (true, true) => true,
        }
    }
}

fn to_timestamp(local: civil::DateTime, zone: &TimeZone) -> Result<Timestamp> {
    Ok(local.to_zoned_datetime(zone.clone())?.timestamp())
}

/// Parse one field into a bitmap; `names` maps three-letter month/weekday
/// aliases. `any` reports a bare `*`, which is needed for cron's day OR rule.
fn parse_field(raw: &str, min: u8, max: u8, names: Option<&[&str]>) -> Result<(u64, bool)> {
    let mut mask = 0u64;
    for item in raw.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, step.parse::<u16>()?),
            None => (item, 1),
        };
        bail_zero_step(step)?;
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((from, to)) = range.split_once('-') {
            (
                parse_value(from, min, max, names)?,
                parse_value(to, min, max, names)?,
            )
        } else {
            let value = parse_value(range, min, max, names)?;
            (value, value)
        };
        ensure!(lo <= hi, "reversed cron range: {range}");
        for value in (lo..=hi).step_by(usize::from(step)) {
            mask |= 1 << if max == 7 && value == 7 { 0 } else { value };
        }
    }
    ensure!(mask != 0, "empty cron field: {raw}");
    Ok((mask, raw == "*"))
}

fn bail_zero_step(step: u16) -> Result<()> {
    ensure!(step > 0, "cron step must be positive");
    Ok(())
}

fn parse_value(raw: &str, min: u8, max: u8, names: Option<&[&str]>) -> Result<u8> {
    let value = if let Some(names) = names {
        match names.iter().position(|name| name.eq_ignore_ascii_case(raw)) {
            Some(index) => u8::try_from(index)? + if max == 12 { 1 } else { 0 },
            None => raw.parse::<u8>()?,
        }
    } else {
        raw.parse::<u8>()?
    };
    ensure!(
        (min..=max).contains(&value),
        "cron value out of range: {raw}"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::Cron;

    fn ms(local: jiff::civil::DateTime, zone: &str) -> i64 {
        local
            .in_tz(zone)
            .expect("valid local time")
            .timestamp()
            .as_millisecond()
    }

    #[test]
    fn cron_next_after_parses_steps_ranges_and_lists() -> anyhow::Result<()> {
        let cron: Cron = "*/15 9-17 * * 1-5".parse()?;
        let monday = date(2024, 6, 3).at(8, 50, 0, 0);
        assert_eq!(
            cron.next_after(ms(monday, "UTC"), "UTC")?,
            Some(ms(date(2024, 6, 3).at(9, 0, 0, 0), "UTC")),
            "the first matching minute after 08:50 is 09:00"
        );
        assert_eq!(
            cron.next_after(ms(date(2024, 6, 3).at(9, 7, 0, 0), "UTC"), "UTC")?,
            Some(ms(date(2024, 6, 3).at(9, 15, 0, 0), "UTC")),
            "*/15 must step through the hour"
        );
        assert_eq!(
            cron.next_after(ms(date(2024, 6, 3).at(17, 50, 0, 0), "UTC"), "UTC")?,
            Some(ms(date(2024, 6, 4).at(9, 0, 0, 0), "UTC")),
            "the hour range restarts on the next matching weekday"
        );

        let list: Cron = "0,30 9-11 * * *".parse()?;
        assert_eq!(
            list.next_after(ms(date(2024, 6, 3).at(10, 5, 0, 0), "UTC"), "UTC")?,
            Some(ms(date(2024, 6, 3).at(10, 30, 0, 0), "UTC"))
        );
        Ok(())
    }

    #[test]
    fn cron_next_after_accepts_sunday_and_crosses_boundaries() -> anyhow::Result<()> {
        let sunday: Cron = "0 0 * * 7".parse()?;
        assert_eq!(
            sunday.next_after(ms(date(2024, 6, 3).at(0, 1, 0, 0), "UTC"), "UTC")?,
            Some(ms(date(2024, 6, 9).at(0, 0, 0, 0), "UTC")),
            "7 is Sunday, not Saturday"
        );

        let yearly: Cron = "0 0 1 1 *".parse()?;
        assert_eq!(
            yearly.next_after(ms(date(2024, 6, 1).at(0, 0, 0, 0), "UTC"), "UTC")?,
            Some(ms(date(2025, 1, 1).at(0, 0, 0, 0), "UTC")),
            "a rare cron must cross month and year boundaries without re-parsing"
        );
        assert_eq!(
            yearly.next_after(ms(date(2024, 1, 1).at(0, 0, 0, 0), "UTC"), "UTC")?,
            Some(ms(date(2025, 1, 1).at(0, 0, 0, 0), "UTC")),
            "the occurrence at `now` is not returned again"
        );

        let impossible: Cron = "0 0 30 2 *".parse()?;
        assert_eq!(
            impossible.next_after(ms(date(2024, 6, 1).at(0, 0, 0, 0), "UTC"), "UTC")?,
            None,
            "a civil date that never exists has no next occurrence"
        );
        Ok(())
    }

    #[test]
    fn cron_next_after_handles_dst_gap_and_fold() -> anyhow::Result<()> {
        let zone = "America/New_York";

        let gap: Cron = "30 2 * * *".parse()?;
        assert_eq!(
            gap.next_after(ms(date(2024, 3, 10).at(1, 50, 0, 0), zone), zone)?,
            Some(ms(date(2024, 3, 10).at(3, 30, 0, 0), zone)),
            "a nonexistent 02:30 resolves to the civil time after the gap"
        );

        let fold: Cron = "30 1 * * *".parse()?;
        assert_eq!(
            fold.next_after(ms(date(2024, 11, 3).at(1, 29, 0, 0), zone), zone)?,
            Some(ms(date(2024, 11, 3).at(1, 30, 0, 0), zone)),
            "the first occurrence of a folded civil time is still due"
        );
        assert_eq!(
            fold.next_after(ms(date(2024, 11, 3).at(1, 50, 0, 0), zone), zone)?,
            Some(ms(date(2024, 11, 4).at(1, 30, 0, 0), zone)),
            "a folded minute already in the past must not fire twice"
        );

        let every_minute: Cron = "* * * * *".parse()?;
        assert_eq!(
            every_minute.next_after(ms(date(2024, 11, 3).at(1, 59, 0, 0), zone), zone)?,
            Some(
                "2024-11-03T07:00:00Z"
                    .parse::<jiff::Timestamp>()?
                    .as_millisecond()
            ),
            "a repeated wall-clock minute is not fired twice: the next minute steps past the fold"
        );
        Ok(())
    }

    #[test]
    fn cron_rejects_malformed_expressions() {
        for bad in [
            "* * * *",
            "60 * * * *",
            "*/0 * * * *",
            "* * * * 8",
            "1-5-9 * * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * MONDAY",
        ] {
            assert!(bad.parse::<Cron>().is_err(), "{bad} must be rejected");
        }
        assert!("0 0 * * mon-fri".parse::<Cron>().is_ok());
    }
}
