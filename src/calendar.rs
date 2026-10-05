//! The proleptic Gregorian calendar over plain day counts, for every `i64` of microseconds: which civil
//! date a day is, and how a moment is written in a message. Worked out here rather than through `chrono`,
//! which refuses — or panics on — moments far outside the range it represents, and a message about a record
//! whose time is wrong must still be written.

pub(crate) const MICROSECONDS_PER_MINUTE: i64 = 60_000_000;
pub(crate) const MICROSECONDS_PER_DAY: i64 = 24 * 60 * MICROSECONDS_PER_MINUTE;

pub(crate) fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days since 1970-01-01 of the civil date `year-month-day`, month 1 ..= 12.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

/// The civil date `(year, month, day)` of the day `days` after 1970-01-01, month 1 ..= 12.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    } as u32;
    let year = year_of_era + era * 400;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `YYYY-MM-DDTHH:MM:SS.ffffff` of a moment given as microseconds since 1970-01-01, whatever the value.
pub(crate) fn format_microseconds(microseconds: i64) -> String {
    let days = microseconds.div_euclid(MICROSECONDS_PER_DAY);
    let within = microseconds.rem_euclid(MICROSECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);

    let seconds = within / 1_000_000;

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        within % 1_000_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days_in_month(year: i64, month: u32) -> u32 {
        match month {
            2 if is_leap_year(year) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        }
    }

    #[test]
    fn a_civil_date_and_its_day_count_round_trip_across_eras() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));

        let mut expected = (1600i64, 1u32, 1u32);
        for days in days_from_civil(1600, 1, 1)..days_from_civil(2401, 1, 1) {
            assert_eq!(civil_from_days(days), expected, "day {days}");
            assert_eq!(days_from_civil(expected.0, expected.1, expected.2), days);

            let (year, month, day) = expected;
            expected = if day < days_in_month(year, month) {
                (year, month, day + 1)
            } else if month < 12 {
                (year, month + 1, 1)
            } else {
                (year + 1, 1, 1)
            };
        }
    }

    #[test]
    fn leap_years_follow_the_gregorian_rule() {
        for (year, leap) in [
            (1900, false),
            (2000, true),
            (2024, true),
            (2026, false),
            (2100, false),
            (0, true),
        ] {
            assert_eq!(is_leap_year(year), leap, "{year}");
        }
    }

    #[test]
    fn any_moment_is_written_out_even_far_outside_chrono() {
        assert_eq!(format_microseconds(0), "1970-01-01T00:00:00.000000");
        assert_eq!(
            format_microseconds(1_789_517_696_123_456),
            "2026-09-16T00:14:56.123456"
        );
        assert_eq!(format_microseconds(-1), "1969-12-31T23:59:59.999999");

        for extreme in [i64::MIN, i64::MAX] {
            assert!(!format_microseconds(extreme).is_empty());
        }
    }
}
