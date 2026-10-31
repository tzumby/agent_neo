//! The user's local date and time, for the model: "yesterday" means the
//! user's yesterday, not the desktop's (which runs on UTC). The app sends its
//! time zone and UTC offset with each message; no time-zone database needed.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq)]
pub struct UserClock {
    /// IANA name, e.g. America/Toronto (shown to the model, not computed with).
    pub timezone: String,
    pub utc_offset_secs: i64,
}

impl Default for UserClock {
    fn default() -> Self {
        Self {
            timezone: "UTC".into(),
            utc_offset_secs: 0,
        }
    }
}

impl UserClock {
    /// Accepts what an app sent, if it's plausible.
    pub fn from_app(timezone: &str, utc_offset_secs: i64) -> Option<Self> {
        let valid_name = !timezone.is_empty()
            && timezone.len() <= 64
            && timezone
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c));
        (valid_name && utc_offset_secs.abs() <= 14 * 3600).then(|| Self {
            timezone: timezone.to_string(),
            utc_offset_secs,
        })
    }

    /// "Saturday 2026-10-31 10:01 (America/Toronto, UTC-04:00)".
    pub fn describe(&self, unix_secs: i64) -> String {
        let local = unix_secs + self.utc_offset_secs;
        let days = local.div_euclid(86_400);
        let minutes = local.rem_euclid(86_400) / 60;
        let (year, month, day) = civil_from_days(days);
        // 1970-01-01 was a Thursday.
        let weekday = [
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
        ][days.rem_euclid(7) as usize];
        let sign = if self.utc_offset_secs < 0 { '-' } else { '+' };
        let offset = self.utc_offset_secs.abs() / 60;
        format!(
            "{weekday} {year:04}-{month:02}-{day:02} {:02}:{:02} ({}, UTC{sign}{:02}:{:02})",
            minutes / 60,
            minutes % 60,
            self.timezone,
            offset / 60,
            offset % 60
        )
    }

    pub fn now(&self) -> String {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.describe(unix)
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian
/// (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_local_time_with_the_offset() {
        let toronto = UserClock::from_app("America/Toronto", -4 * 3600).unwrap();
        // 2026-10-31 14:01 UTC = 10:01 in Toronto, a Saturday.
        assert_eq!(
            toronto.describe(1_793_455_260),
            "Saturday 2026-10-31 10:01 (America/Toronto, UTC-04:00)"
        );
        // 02:30 UTC on Nov 1 is still Oct 31 in Toronto: its "yesterday" is the 30th.
        assert!(
            toronto
                .describe(1_793_500_200)
                .starts_with("Saturday 2026-10-31 22:30")
        );
        let india = UserClock::from_app("Asia/Kolkata", 19_800).unwrap();
        assert!(india.describe(0).starts_with("Thursday 1970-01-01 05:30"));
        assert!(
            UserClock::default()
                .describe(951_782_400)
                .starts_with("Tuesday 2000-02-29 00:00")
        );
    }

    #[test]
    fn rejects_implausible_clocks() {
        assert!(UserClock::from_app("", 0).is_none());
        assert!(UserClock::from_app("Europe/London; rm -rf /", 0).is_none());
        assert!(UserClock::from_app("UTC", 15 * 3600).is_none());
    }
}
