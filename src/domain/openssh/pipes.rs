use std::{
    io::{self, BufRead, BufReader, Read},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use super::OpenSshError;
use crate::platform::process::terminate_process_group;

/// Appends `line`, then drops whole lines from the front until at most `limit` bytes remain,
/// so a chatty ssh keeps its newest output. A single line over the limit keeps its newest
/// `limit` bytes instead.
pub(super) fn push_capped(stderr: &mut String, line: &str, limit: usize) {
    stderr.push_str(line);
    stderr.push('\n');
    if let Some(excess) = stderr.len().checked_sub(limit) {
        let whole_lines = stderr.as_bytes()[excess..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|at| excess + at + 1)
            .filter(|&cut| cut < stderr.len());
        let cut = whole_lines.unwrap_or_else(|| {
            (excess..=stderr.len())
                .find(|&at| stderr.is_char_boundary(at))
                .unwrap_or(stderr.len())
        });
        stderr.drain(..cut);
    }
}

/// Sends each line of `pipe` as it arrives; the channel closes at end of file.
pub(super) fn read_lines(pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        while reader
            .read_until(b'\n', &mut line)
            .is_ok_and(|read| read > 0)
        {
            let text = String::from_utf8_lossy(&line);
            if sender
                .send(text.trim_end_matches(['\n', '\r']).to_string())
                .is_err()
            {
                return;
            }
            line.clear();
        }
    });
    receiver
}

pub(super) fn read_pipe(
    mut pipe: impl Read + Send + 'static,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn finish_pipe(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    program: &Path,
) -> Result<Vec<u8>, OpenSshError> {
    reader
        .join()
        .map_err(|_| OpenSshError::Io {
            program: program.into(),
            source: io::Error::other("output reader panicked"),
        })?
        .map_err(|source| OpenSshError::Io {
            program: program.into(),
            source,
        })
}

pub(super) fn finish_pipe_cancellable(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    child: &mut std::process::Child,
    cancelled: &AtomicBool,
    program: &Path,
) -> Result<Vec<u8>, OpenSshError> {
    while !reader.is_finished() {
        if cancelled.load(Ordering::Relaxed) {
            terminate_process_group(child);
            return Err(OpenSshError::Cancelled);
        }
        thread::sleep(Duration::from_millis(20));
    }
    match finish_pipe(reader, program) {
        Ok(output) => Ok(output),
        Err(error) => {
            terminate_process_group(child);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::probe::TEST_STDERR_LIMIT;

    #[test]
    fn captured_stderr_keeps_the_newest_whole_lines_within_the_limit() {
        let mut stderr = String::new();
        let line = "x".repeat(1023);
        for n in 0..400 {
            push_capped(&mut stderr, &format!("{n}{line}"), TEST_STDERR_LIMIT);
        }
        assert!(stderr.len() <= TEST_STDERR_LIMIT);
        assert!(stderr.ends_with(&format!("399{line}\n")));
        assert!(stderr.lines().all(|l| l.ends_with(&line)));
        assert!(!stderr.starts_with("0x"));
    }

    #[test]
    fn a_line_over_the_limit_keeps_its_newest_bytes() {
        let mut stderr = String::from("old\n");
        push_capped(&mut stderr, &format!("{}tail", "\u{e9}".repeat(20)), 11);
        assert!(stderr.len() <= 11);
        assert!(stderr.ends_with("tail\n"));
        assert!(!stderr.contains("old"));
    }
}
