//! Windows local time.

use windows_sys::Win32::{
    Foundation::{FILETIME, SYSTEMTIME},
    System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
};

use super::super::common::time::LocalTime;

/// `epoch_secs` (seconds since 1970) as the local calendar time, or `None` when it cannot be
/// represented. It converts as the C runtime's `localtime_s` would, through Windows calls
/// (`windows-sys` has no C runtime): the current time zone's rules apply.
pub fn local(epoch_secs: i64) -> Option<LocalTime> {
    const SECONDS_FROM_1601_TO_1970: i64 = 11_644_473_600;
    let ticks = epoch_secs
        .checked_add(SECONDS_FROM_1601_TO_1970)?
        .checked_mul(10_000_000)?;
    let ticks = u64::try_from(ticks).ok()?;
    let file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: each call reads and writes only the structures it is given; a null time zone
    // means the current one.
    let converted = unsafe {
        FileTimeToSystemTime(&file_time, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
    };
    if !converted {
        return None;
    }
    Some(LocalTime {
        year: i32::from(local.wYear),
        month: u8::try_from(local.wMonth).ok()?,
        day: u8::try_from(local.wDay).ok()?,
        hour: u8::try_from(local.wHour).ok()?,
        minute: u8::try_from(local.wMinute).ok()?,
        second: u8::try_from(local.wSecond).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_in_1969_or_1970_whatever_the_time_zone() {
        let epoch = local(0).unwrap();
        assert!(matches!(epoch.year, 1969 | 1970));
        assert!(epoch.hour < 24 && epoch.minute < 60);
    }

    #[test]
    fn a_time_too_large_for_the_system_has_no_local_time() {
        assert_eq!(local(i64::MAX), None);
    }

    #[test]
    fn a_known_time_has_the_expected_minutes_and_seconds() {
        // 2001-09-09 01:46:40 UTC; every time zone is a whole number of quarter hours off.
        let time = local(1_000_000_000).unwrap();
        assert_eq!(time.second, 40);
        assert!(matches!(time.minute, 1 | 16 | 31 | 46));
    }
}
