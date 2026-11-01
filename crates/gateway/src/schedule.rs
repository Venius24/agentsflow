//! Schedules: `[[schedule]]` of a department puts a task into the queue on a
//! cron expression. The last firing of each schedule is kept in
//! `.agentsflow/schedules.json`, so a restart neither repeats nor loses one:
//! a firing missed while the server was down happens once on start.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// A five-field cron expression: minute hour day-of-month month day-of-week.
#[derive(Clone, Debug, PartialEq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<u64> {
    let value = |part: &str| -> Result<u32> {
        let lower = part.to_ascii_lowercase();
        if let Some(index) = names.iter().position(|name| *name == lower) {
            return Ok(index as u32 + min);
        }
        part.parse::<u32>()
            .ok()
            .filter(|n| (min..=max).contains(n))
            .with_context(|| format!("{part:?} вне {min}..{max}"))
    };
    let mut mask = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .ok()
                    .filter(|s| *s > 0)
                    .with_context(|| format!("неверный шаг {step:?}"))?,
            ),
            None => (item, 1),
        };
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let start = value(range)?;
            (start, if item.contains('/') { max } else { start })
        };
        if from > to {
            bail!("диапазон {range:?} задом наперёд");
        }
        let mut n = from;
        while n <= to {
            mask |= 1 << n;
            n += step;
        }
    }
    Ok(mask)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self> {
        let text = match text.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            bail!("cron из пяти полей: минута час день месяц день-недели");
        };
        let mut weekdays = field(weekday, 0, 7, &WEEKDAYS)?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & 0x7f;
        }
        Ok(Self {
            minutes: field(minute, 0, 59, &[])?,
            hours: field(hour, 0, 23, &[])? as u32,
            days: field(day, 1, 31, &[])? as u32,
            months: field(month, 1, 12, &MONTHS)? as u16,
            weekdays: weekdays as u8,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    fn day_matches(&self, day: u32, weekday: u32) -> bool {
        let by_day = self.days & (1 << day) != 0;
        let by_weekday = self.weekdays & (1 << weekday) != 0;
        // Classic cron: when both are restricted, either one matches.
        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (true, false) => by_weekday,
            (false, true) => by_day,
            (false, false) => by_day || by_weekday,
        }
    }

    /// The first firing strictly after `after_ms`, in local time shifted by
    /// `offset_min` from UTC.
    pub fn next_after(&self, after_ms: u64, offset_min: i32) -> Option<u64> {
        let mut local = (after_ms / 60_000) as i64 + 1 + i64::from(offset_min);
        let limit = local + 5 * 366 * 1440;
        while local < limit {
            let day = local.div_euclid(1440);
            let minute_of_day = local.rem_euclid(1440);
            let (_, month, date) = civil(day);
            let weekday = (day + 4).rem_euclid(7) as u32;
            if self.months & (1 << month) == 0 || !self.day_matches(date, weekday) {
                local = (day + 1) * 1440;
                continue;
            }
            let hour = (minute_of_day / 60) as u32;
            if self.hours & (1 << hour) == 0 {
                local = day * 1440 + (i64::from(hour) + 1) * 60;
                continue;
            }
            if self.minutes & (1 << (minute_of_day % 60)) == 0 {
                local += 1;
                continue;
            }
            let utc = local - i64::from(offset_min);
            return u64::try_from(utc * 60_000).ok();
        }
        None
    }
}

/// Civil date from days since the epoch (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// `+05:00` → 300.
pub fn parse_offset(text: &str) -> Result<i32> {
    let text = text.trim();
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'+') => (1, &text[1..]),
        Some(b'-') => (-1, &text[1..]),
        _ => bail!("смещение вида +05:00"),
    };
    let (hours, minutes) = rest.split_once(':').unwrap_or((rest, "0"));
    let hours: i32 = hours.parse().context("смещение вида +05:00")?;
    let minutes: i32 = minutes.parse().context("смещение вида +05:00")?;
    if !(0..=14).contains(&hours) || !(0..60).contains(&minutes) {
        bail!("смещение вне -14:00..+14:00");
    }
    Ok(sign * (hours * 60 + minutes))
}

/// A stable key of a schedule: department plus a hash of what it does.
pub fn key(department: &str, cron: &str, task: &str) -> String {
    // FNV-1a: stable across builds, unlike the std hasher.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in cron.bytes().chain([0]).chain(task.bytes()) {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{department}-{hash:08x}")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub last_ms: Option<u64>,
    #[serde(default)]
    pub paused: bool,
}

/// Last firings and pauses, saved after every change.
#[derive(Debug, Default)]
pub struct State {
    path: Option<PathBuf>,
    pub entries: BTreeMap<String, Entry>,
}

impl State {
    pub fn open(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries,
        }
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let result = serde_json::to_vec_pretty(&self.entries)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(error) = result {
            crate::server_diag!("schedules save failed: {error}");
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Info {
    pub key: String,
    pub department: String,
    pub title: String,
    pub cron: String,
    pub paused: bool,
    pub last_ms: Option<u64>,
    pub next_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-02 is a Monday.
    const MONDAY_UTC: u64 = 1_793_577_600_000;

    fn at(minutes: u64) -> u64 {
        MONDAY_UTC + minutes * 60_000
    }

    #[test]
    fn parses_and_finds_next_firings() {
        assert_eq!(civil((MONDAY_UTC / 86_400_000) as i64), (2026, 11, 2));
        let daily = Cron::parse("30 9 * * *").unwrap();
        assert_eq!(daily.next_after(at(0), 0), Some(at(9 * 60 + 30)));
        // At 09:30 exactly the next one is tomorrow.
        assert_eq!(
            daily.next_after(at(9 * 60 + 30), 0),
            Some(at(24 * 60 + 9 * 60 + 30))
        );
        // 09:30 at +05:00 is 04:30 UTC.
        assert_eq!(daily.next_after(at(0), 300), Some(at(4 * 60 + 30)));
        let weekdays = Cron::parse("0 10 * * mon-fri").unwrap();
        // Saturday 2026-11-07 → next is Monday 2026-11-09.
        assert_eq!(
            weekdays.next_after(at(5 * 1440), 0),
            Some(at(7 * 1440 + 600))
        );
        let every = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(every.next_after(at(1), 0), Some(at(15)));
        let monthly = Cron::parse("@monthly").unwrap();
        assert_eq!(monthly.next_after(at(0), 0), Some(1_796_083_200_000));
        let sunday = Cron::parse("0 8 * * 7").unwrap();
        assert_eq!(sunday.next_after(at(0), 0), Some(at(6 * 1440 + 480)));
        let either = Cron::parse("0 0 1 * mon").unwrap();
        assert_eq!(either.next_after(at(1), 0), Some(at(7 * 1440)));
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* * * * fun",
            "5-1 * * * *",
            "*/0 * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn offsets_and_keys() {
        assert_eq!(parse_offset("+05:00").unwrap(), 300);
        assert_eq!(parse_offset("-03:30").unwrap(), -210);
        assert!(parse_offset("05:00").is_err());
        assert!(parse_offset("+15:00").is_err());
        assert_eq!(key("pr", "0 9 * * *", "a"), key("pr", "0 9 * * *", "a"));
        assert_ne!(key("pr", "0 9 * * *", "a"), key("pr", "0 9 * * *", "b"));
    }
}
