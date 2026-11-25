use anyhow::{Context, Result};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

/// The machine's UTC offset at an instant, so a daemon that runs across a
/// daylight saving change still stamps each entry with the offset of its day.
/// `localtime_r` is the thread-safe libc call; `time`'s own local offset is
/// refused once a process has threads.
pub fn offset_at(seconds: i64) -> UtcOffset {
    let instant = seconds as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::zeroed();
    let filled = unsafe { libc::localtime_r(&instant, local.as_mut_ptr()) };
    assert!(!filled.is_null(), "localtime_r rejected timestamp {seconds}");
    let gmtoff = unsafe { local.assume_init() }.tm_gmtoff;
    UtcOffset::from_whole_seconds(gmtoff as i32).expect("libc returns a UTC offset under a day")
}

fn local(seconds: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .map(|moment| moment.to_offset(offset_at(seconds)))
}

/// The local calendar date, as `YYYY-MM-DD`.
pub fn local_date(seconds: i64) -> String {
    local(seconds).map(|moment| moment.date().to_string()).unwrap_or_default()
}

pub fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// ISO 8601 with the local offset, which is what Chronoid stores entries in.
pub fn to_iso(seconds: i64) -> Result<String> {
    local(seconds)
        .context("convert a timestamp")?
        .format(&Rfc3339)
        .context("format a timestamp")
}

/// The first instant in `(low, high]` whose local date is `date` or later,
/// given that `low` falls before `date` and `high` on or after it. Searched
/// rather than computed, because where clocks change at midnight a local
/// midnight can repeat or not exist at all.
fn first_instant_of(date: Date, mut low: i64, mut high: i64) -> i64 {
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if local(middle).is_some_and(|moment| moment.date() >= date) {
            high = middle;
        } else {
            low = middle;
        }
    }
    high
}

/// The first instant of a local day.
pub fn start_of_day(date: Date) -> i64 {
    // UTC offsets run from -12 to +14 hours, so local midnight lies within 15
    // hours of the same wall time in UTC.
    let naive = date.midnight().assume_utc().unix_timestamp();
    first_instant_of(date, naive - 15 * 3600, naive + 15 * 3600)
}

/// The first instant of the next local day, always later than `seconds`.
/// Entries are cut here so a block that runs past midnight does not smear
/// across two days' reports.
pub fn next_midnight(seconds: i64) -> i64 {
    let Some(moment) = local(seconds) else {
        return seconds + 1;
    };
    let date = moment.date();
    let tomorrow = date.next_day().unwrap_or(date);
    // No local day is longer than 26 hours.
    first_instant_of(tomorrow, seconds, seconds + 27 * 3600)
}

pub fn same_local_day(a: i64, b: i64) -> bool {
    next_midnight(a) == next_midnight(b)
}

pub fn duration(seconds: i64) -> String {
    let minutes = seconds / 60;
    if minutes >= 60 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_is_local_midnight_on_every_day_of_the_year() {
        // 2026-01-01T12:00:00Z, then one sample per day, crossing both
        // daylight saving changes in zones that have them.
        let start = 1_767_268_800;
        for day in 0..366 {
            let sample = start + day * 86_400;
            let midnight = next_midnight(sample);
            assert!(midnight > sample);
            let today = local(sample).unwrap().date();
            assert_eq!(local(midnight).unwrap().date(), today.next_day().unwrap(), "sample {sample}");
            assert_eq!(local(midnight - 1).unwrap().date(), today, "sample {sample}");
            assert_eq!(start_of_day(today.next_day().unwrap()), midnight);
            assert!(same_local_day(sample, midnight - 1));
            assert!(!same_local_day(sample, midnight));
        }
    }

    #[test]
    fn stamps_each_instant_with_the_offset_of_its_own_day() {
        // Either side of the 2026-11-01 US change, in whatever zone runs this.
        let before = 1_790_000_000; // 2026-09-21
        let after = 1_795_000_000; // 2026-11-18
        for seconds in [before, after] {
            let iso = to_iso(seconds).unwrap();
            let parsed = OffsetDateTime::parse(&iso, &Rfc3339).unwrap();
            assert_eq!(parsed.unix_timestamp(), seconds);
            assert_eq!(parsed.offset(), offset_at(seconds));
        }
    }
}
