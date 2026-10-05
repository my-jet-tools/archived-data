use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::{
    ArchiveError, MICROSECONDS_PER_DAY, civil_from_days, days_from_civil, is_leap_year, le_u16,
};

/// `year u16 · 46 bytes of days`.
pub(crate) const YEAR_BLOCK_LEN: usize = 48;

/// 366 days in bits: the last two bits of the last byte are always 0.
const YEAR_BITS_LEN: usize = 46;

/// The positions a day has: those of a leap year.
const DAYS_IN_LEAP_YEAR: u16 = 366;

/// Where each month starts in the calendar of a leap year.
const LEAP_MONTH_STARTS: [u16; 12] = [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335];

/// 29 February, which a common year never sets.
const LEAP_DAY: u16 = 59;

/// The days of one year a login has data on. A day is its position in the calendar of a LEAP year — 1 January
/// is 0, 29 February 59, 1 March always 60 — so a date has the same position in every year, and a common
/// year never sets 59. Day `d` is bit `1 << (d % 8)` of byte `d / 8`: day 0 is `0x01` of the first byte, day
/// 7 its `0x80`, day 8 `0x01` of the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YearDays {
    pub year: u16,
    bits: [u8; YEAR_BITS_LEN],
}

impl YearDays {
    pub fn new(year: u16) -> Self {
        Self {
            year,
            bits: [0; YEAR_BITS_LEN],
        }
    }

    /// Sets `day`, a position below 366; setting it again changes nothing.
    pub fn set(&mut self, day: u16) {
        self.bits[day as usize / 8] |= 1 << (day % 8);
    }

    /// Every day set, ascending.
    pub fn days(&self) -> impl Iterator<Item = u16> + '_ {
        (0..DAYS_IN_LEAP_YEAR).filter(|day| self.bits[*day as usize / 8] & (1 << (day % 8)) != 0)
    }

    pub fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.year.to_le_bytes());
        out.extend_from_slice(&self.bits);
    }

    /// The block in the first [`YEAR_BLOCK_LEN`] bytes of `bytes`, read at `offset`. A block with no day set,
    /// a bit past day 365, or 29 February set in a common year is `Corrupted`.
    pub fn decode(bytes: &[u8], offset: u64) -> Result<Self, ArchiveError> {
        let mut block = Self::new(le_u16(bytes, 0));
        block.bits.copy_from_slice(&bytes[2..YEAR_BLOCK_LEN]);

        let past_the_year = block.bits[YEAR_BITS_LEN - 1] & 0b1100_0000 != 0;
        let leap_day_of_a_common_year =
            !is_leap_year(block.year as i64) && block.days().any(|day| day == LEAP_DAY);

        if block.bits == [0; YEAR_BITS_LEN] || past_the_year || leap_day_of_a_common_year {
            return Err(ArchiveError::Corrupted(format!(
                "the year block at {offset} for {} sets no day, a day past 365, or 29 February of a common year",
                block.year
            )));
        }

        Ok(block)
    }

    /// The `(year, position)` of the day `microseconds` falls in; `None` for a year a `u16` cannot hold.
    pub fn day_of(microseconds: i64) -> Option<(u16, u16)> {
        let (year, month, day) = civil_from_days(microseconds.div_euclid(MICROSECONDS_PER_DAY));
        let year = u16::try_from(year).ok()?;

        Some((year, LEAP_MONTH_STARTS[month as usize - 1] + day as u16 - 1))
    }

    /// The first microsecond of every day set from the day of `from` to the day of `to`, both included,
    /// ascending.
    pub fn starts_between(&self, from: i64, to: i64) -> impl Iterator<Item = i64> + '_ {
        let (first, last) = (
            from.div_euclid(MICROSECONDS_PER_DAY),
            to.div_euclid(MICROSECONDS_PER_DAY),
        );

        self.days()
            .map(|day| {
                let month = LEAP_MONTH_STARTS.partition_point(|start| *start <= day);
                let within = (day - LEAP_MONTH_STARTS[month - 1] + 1) as u32;
                days_from_civil(self.year as i64, month as u32, within)
            })
            .filter(move |days| (first..=last).contains(days))
            .map(|days| days * MICROSECONDS_PER_DAY)
    }
}

/// The year blocks in `bytes`, read at `offset`: whole blocks, each decoded as [`YearDays::decode`] checks it,
/// in strictly ascending years.
pub(crate) fn decode_year_blocks(bytes: &[u8], offset: u64) -> Result<Vec<YearDays>, ArchiveError> {
    let mut blocks: Vec<YearDays> = Vec::with_capacity(bytes.len() / YEAR_BLOCK_LEN);

    for (index, block) in bytes.chunks(YEAR_BLOCK_LEN).enumerate() {
        let at = offset + (index * YEAR_BLOCK_LEN) as u64;

        if block.len() != YEAR_BLOCK_LEN {
            return Err(ArchiveError::Corrupted(format!(
                "the year blocks at {offset} end in {} bytes of a block",
                block.len()
            )));
        }

        let block = YearDays::decode(block, at)?;

        if blocks
            .last()
            .is_some_and(|previous| previous.year >= block.year)
        {
            return Err(ArchiveError::Corrupted(format!(
                "the year block at {at} for {} does not follow the one before it",
                block.year
            )));
        }

        blocks.push(block);
    }

    Ok(blocks)
}

/// Every day of `blocks` from the day of `from` to the day of `to`, as the first microsecond of each.
pub(crate) fn days_between(
    blocks: &[YearDays],
    from: DateTimeAsMicroseconds,
    to: DateTimeAsMicroseconds,
) -> Vec<DateTimeAsMicroseconds> {
    let (from, to) = (from.unix_microseconds, to.unix_microseconds);

    if from > to {
        return Vec::new();
    }

    let first_year = civil_from_days(from.div_euclid(MICROSECONDS_PER_DAY)).0;
    let last_year = civil_from_days(to.div_euclid(MICROSECONDS_PER_DAY)).0;

    blocks
        .iter()
        .filter(|block| (first_year..=last_year).contains(&(block.year as i64)))
        .flat_map(|block| block.starts_between(from, to))
        .map(DateTimeAsMicroseconds::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(year: i64, month: u32, day: u32) -> i64 {
        days_from_civil(year, month, day) * MICROSECONDS_PER_DAY
    }

    #[test]
    fn a_day_is_its_place_in_a_leap_year_whatever_the_year() {
        for (year, month, day, expected) in [
            (2024, 1, 1, 0),
            (2024, 1, 31, 30),
            (2024, 2, 28, 58),
            (2024, 2, 29, 59),
            (2024, 3, 1, 60),
            (2024, 12, 31, 365),
            (2026, 2, 28, 58),
            (2026, 3, 1, 60),
            (2026, 12, 31, 365),
            (2100, 3, 1, 60),
        ] {
            let at = start(year, month, day);
            assert_eq!(
                YearDays::day_of(at + MICROSECONDS_PER_DAY - 1),
                Some((year as u16, expected)),
                "{year}-{month}-{day}"
            );
        }

        assert_eq!(YearDays::day_of(start(-1, 6, 1)), None);
        assert_eq!(YearDays::day_of(start(65_536, 1, 1)), None);
        assert_eq!(YearDays::day_of(start(65_535, 12, 31)), Some((65_535, 365)));
    }

    #[test]
    fn day_d_is_bit_d_mod_8_of_byte_d_div_8_lowest_bit_first() {
        let mut block = YearDays::new(2026);

        for day in [0, 7, 8, 60, 365] {
            block.set(day);
        }
        block.set(7);

        let mut bytes = Vec::new();
        block.encode_to(&mut bytes);

        let mut expected = vec![0u8; 48];
        expected[..2].copy_from_slice(&2026u16.to_le_bytes());
        expected[2] = 0x81;
        expected[3] = 0x01;
        expected[2 + 7] = 0x10;
        expected[2 + 45] = 0x20;
        assert_eq!(bytes, expected);

        assert_eq!(block.days().collect::<Vec<_>>(), [0, 7, 8, 60, 365]);
        assert_eq!(YearDays::decode(&bytes, 0).unwrap(), block);
    }

    #[test]
    fn a_block_setting_nothing_past_365_or_29_february_of_a_common_year_is_corrupted() {
        let encoded = |year: u16, day: Option<u16>, last_byte: u8| {
            let mut block = YearDays::new(year);
            if let Some(day) = day {
                block.set(day);
            }
            let mut bytes = Vec::new();
            block.encode_to(&mut bytes);
            bytes[47] |= last_byte;
            bytes
        };

        assert!(YearDays::decode(&encoded(2024, Some(59), 0), 0).is_ok());

        for (case, bytes) in [
            ("nothing set", encoded(2024, None, 0)),
            ("day 366", encoded(2024, Some(1), 0x40)),
            ("day 367", encoded(2024, Some(1), 0x80)),
            ("29 February 2026", encoded(2026, Some(59), 0)),
            ("29 February 2100", encoded(2100, Some(59), 0)),
        ] {
            assert!(
                matches!(YearDays::decode(&bytes, 0), Err(ArchiveError::Corrupted(_))),
                "{case}"
            );
        }

        let mut two = encoded(2026, Some(3), 0);
        two.extend(encoded(2025, Some(3), 0));
        assert!(decode_year_blocks(&two, 0).is_err(), "years out of order");

        let mut same = encoded(2026, Some(3), 0);
        same.extend(encoded(2026, Some(4), 0));
        assert!(decode_year_blocks(&same, 0).is_err(), "a year twice");
        assert!(
            decode_year_blocks(&same[..60], 0).is_err(),
            "a block cut short"
        );
    }

    #[test]
    fn the_days_between_two_moments_include_both_of_their_days() {
        let mut blocks = Vec::new();

        for year in [2024u16, 2025, 2026] {
            let mut block = YearDays::new(year);
            for day in [0, 58, 59, 60, 365] {
                if day != 59 || is_leap_year(year as i64) {
                    block.set(day);
                }
            }
            blocks.push(block);
        }

        let all = days_between(
            &blocks,
            DateTimeAsMicroseconds::new(i64::MIN),
            DateTimeAsMicroseconds::new(i64::MAX),
        );
        let expected: Vec<i64> = [
            (2024, 1, 1),
            (2024, 2, 28),
            (2024, 2, 29),
            (2024, 3, 1),
            (2024, 12, 31),
            (2025, 1, 1),
            (2025, 2, 28),
            (2025, 3, 1),
            (2025, 12, 31),
            (2026, 1, 1),
            (2026, 2, 28),
            (2026, 3, 1),
            (2026, 12, 31),
        ]
        .into_iter()
        .map(|(year, month, day)| start(year, month, day))
        .collect();

        assert_eq!(
            all.iter()
                .map(|day| day.unix_microseconds)
                .collect::<Vec<_>>(),
            expected
        );

        let between = |from: i64, to: i64| {
            days_between(
                &blocks,
                DateTimeAsMicroseconds::new(from),
                DateTimeAsMicroseconds::new(to),
            )
            .into_iter()
            .map(|day| day.unix_microseconds)
            .collect::<Vec<_>>()
        };

        // The last microsecond of 31 December 2024 to the first of 1 January 2025.
        assert_eq!(
            between(start(2025, 1, 1) - 1, start(2025, 1, 1)),
            [start(2024, 12, 31), start(2025, 1, 1)]
        );
        assert_eq!(
            between(start(2025, 2, 28) + 5, start(2025, 3, 1)),
            [start(2025, 2, 28), start(2025, 3, 1)]
        );
        assert!(between(start(2025, 1, 2), start(2025, 2, 27)).is_empty());
        assert!(between(start(2026, 1, 1), start(2025, 1, 1)).is_empty());
    }
}
