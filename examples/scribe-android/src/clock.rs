//! Wall-clock formatting in the phone's own timezone, without a date library.

/// A broken-down local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    pub year: i32,
    /// 1..=12
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Days since the Unix epoch in local time, for "today" and "yesterday".
    pub day_number: i64,
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `unix` in the device's timezone. bionic reads the system zone itself, so no TZ setup.
pub fn local(unix: i64) -> Local {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = unix as libc::time_t;
    unsafe { libc::localtime_r(&time, &mut tm) };
    Local {
        year: tm.tm_year + 1900,
        month: tm.tm_mon as u32 + 1,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
        day_number: (unix + tm.tm_gmtoff as i64).div_euclid(86_400),
    }
}

impl Local {
    /// A sortable id: `20261110-081522`.
    pub fn stamp(&self) -> String {
        format!("{:04}{:02}{:02}-{:02}{:02}{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// `8:15 AM`
    pub fn time(&self) -> String {
        let (hour, half) = match self.hour {
            0 => (12, "AM"),
            h @ 1..=11 => (h, "AM"),
            12 => (12, "PM"),
            h => (h - 12, "PM"),
        };
        format!("{hour}:{:02} {half}", self.minute)
    }

    /// `Nov 10`
    pub fn date(&self) -> String {
        format!("{} {}", MONTHS[(self.month as usize - 1).min(11)], self.day)
    }

    /// How a note's date reads next to `now`: the day by name when it is recent, the year
    /// only when it is not this one.
    pub fn relative_to(&self, now: &Local) -> String {
        match now.day_number - self.day_number {
            0 => format!("Today {}", self.time()),
            1 => format!("Yesterday {}", self.time()),
            _ if self.year == now.year => format!("{}, {}", self.date(), self.time()),
            _ => format!("{} {}", self.date(), self.year),
        }
    }
}

/// `12:34`, or `1:02:03` past an hour.
pub fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32, day_number: i64) -> Local {
        Local { year, month, day, hour, minute, second: 5, day_number }
    }

    #[test]
    fn twelve_hour_clock_handles_midnight_and_noon() {
        assert_eq!(at(2026, 11, 10, 0, 5, 0).time(), "12:05 AM");
        assert_eq!(at(2026, 11, 10, 8, 15, 0).time(), "8:15 AM");
        assert_eq!(at(2026, 11, 10, 12, 0, 0).time(), "12:00 PM");
        assert_eq!(at(2026, 11, 10, 23, 59, 0).time(), "11:59 PM");
    }

    #[test]
    fn dates_read_relative_to_today() {
        let now = at(2026, 11, 10, 9, 0, 1000);
        assert_eq!(at(2026, 11, 10, 8, 15, 1000).relative_to(&now), "Today 8:15 AM");
        assert_eq!(at(2026, 11, 9, 16, 2, 999).relative_to(&now), "Yesterday 4:02 PM");
        assert_eq!(at(2026, 11, 3, 8, 15, 993).relative_to(&now), "Nov 3, 8:15 AM");
        assert_eq!(at(2025, 12, 31, 8, 15, 686).relative_to(&now), "Dec 31 2025");
    }

    #[test]
    fn stamps_sort_and_durations_read() {
        assert_eq!(at(2026, 1, 2, 3, 4, 0).stamp(), "20260102-030405");
        assert_eq!(duration(0.0), "0:00");
        assert_eq!(duration(754.9), "12:34");
        assert_eq!(duration(3723.0), "1:02:03");
        assert_eq!(duration(-5.0), "0:00");
    }

    #[test]
    fn local_time_is_consistent_with_its_day_number() {
        let unix = 1_794_300_000;
        let (a, b) = (local(unix), local(unix + 86_400));
        // A day later is the next local day, whatever the zone (no DST change at this date).
        assert_eq!(b.day_number - a.day_number, 1);
        assert_eq!((a.hour, a.minute), (b.hour, b.minute));
    }
}
