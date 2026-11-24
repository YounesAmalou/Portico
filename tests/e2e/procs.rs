//! The process table on Linux and macOS, for the tests: whether a process exists or runs, its
//! start time, its arguments and its processor time.

use std::time::Duration;

use portico::platform::process::Proc;

/// Whether `pid` exists at all, a zombie included.
pub fn exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether `pid` exists and is not a zombie.
pub fn running(pid: u32) -> bool {
    Proc::system()
        .stat(pid)
        .is_some_and(|stat| stat.state != 'Z')
}

/// The start time of `pid` as a tunnel record stores it, and this boot's ID.
pub fn start_and_boot(pid: u32) -> (u64, String) {
    let proc = Proc::system();
    (proc.stat(pid).unwrap().start, proc.boot_id().unwrap())
}

/// The processes whose arguments satisfy `matches`.
pub fn with_args(matches: impl Fn(&[Vec<u8>]) -> bool) -> Vec<u32> {
    let proc = Proc::system();
    proc.pids()
        .into_iter()
        .filter(|pid| proc.cmdline(*pid).is_some_and(|args| matches(&args)))
        .collect()
}

/// The user and system time of `pid`, all its threads together; zero once it is gone.
#[cfg(target_os = "linux")]
pub fn cpu_time(pid: u32) -> Duration {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return Duration::ZERO;
    };
    // The command name may hold spaces; the fields after it do not.
    let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .collect();
    // utime and stime are the 14th and 15th fields, so the 12th and 13th here.
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    // SAFETY: `sysconf` only reads a configuration value.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    Duration::from_millis(ticks * 1000 / per_second)
}

/// The user and system time of `pid`, all its threads together; zero once it is gone.
/// `ps` prints it as `[[days-]hours:]minutes:seconds.hundredths`.
#[cfg(not(target_os = "linux"))]
pub fn cpu_time(pid: u32) -> Duration {
    let output = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Duration::ZERO;
    }
    let (days, clock) = text.split_once('-').unwrap_or(("0", text));
    let seconds = clock.split(':').fold(0.0, |total, part| {
        total * 60.0 + part.parse::<f64>().unwrap()
    });
    Duration::from_secs_f64(days.parse::<f64>().unwrap() * 86_400.0 + seconds)
}
