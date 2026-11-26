//! Unix local time (Linux and macOS).

use super::super::common::time::LocalTime;

/// `epoch_secs` (seconds since 1970) as the local calendar time, or `None` when it cannot be
/// represented.
pub fn local(epoch_secs: i64) -> Option<LocalTime> {
    // `time_t` is inferred: naming it is deprecated on musl, where it changes size.
    #[allow(clippy::useless_conversion)] // `time_t` is `i64` on 64-bit systems only.
    let time = epoch_secs.try_into().ok()?;
    // SAFETY: `localtime_r` only writes the `tm` it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return None;
        }
        tm
    };
    Some(LocalTime {
        year: tm.tm_year + 1900,
        month: u8::try_from(tm.tm_mon + 1).ok()?,
        day: u8::try_from(tm.tm_mday).ok()?,
        hour: u8::try_from(tm.tm_hour).ok()?,
        minute: u8::try_from(tm.tm_min).ok()?,
        second: u8::try_from(tm.tm_sec).ok()?,
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
}
