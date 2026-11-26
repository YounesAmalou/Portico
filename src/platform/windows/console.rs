//! The console's input queue, read beneath crossterm for the keys crossterm cannot place.
//!
//! A terminal that hosts the console through ConPTY sends keys as text, and ConPTY turns each
//! character into a key press whose virtual key is often unknown (0), followed by a release
//! that names it. crossterm finds no key for a control character without its virtual key and
//! drops the press, so Enter, Esc, Tab, Backspace and Ctrl+letter would arrive only as their
//! release.

use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::io::AsRawHandle,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0},
    System::{
        Console::{
            INPUT_RECORD, KEY_EVENT, LEFT_ALT_PRESSED, PeekConsoleInputW, RIGHT_ALT_PRESSED,
            ReadConsoleInputW, SHIFT_PRESSED,
        },
        Threading::WaitForSingleObject,
    },
};

/// A key press the console reported by its character alone: a control character or DEL, with
/// no virtual key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BareKey {
    pub character: u8,
    pub alt: bool,
    pub shift: bool,
}

/// The console's input, as crossterm opens it (`CONIN$`), whatever standard input is.
fn console_input() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("CONIN$")
}

fn handle(file: &File) -> HANDLE {
    file.as_raw_handle()
}

/// Waits at most `timeout` for input to be queued; whether some is.
pub fn wait_for_input(timeout: Duration) -> io::Result<bool> {
    let input = console_input()?;
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: a console input handle owned by `input`, open for the call.
    match unsafe { WaitForSingleObject(handle(&input), millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ => Ok(false),
    }
}

/// Takes the next input record off the queue when it is a bare key press, leaving any other
/// record where it is.
pub fn take_bare_key() -> io::Result<Option<BareKey>> {
    let input = console_input()?;
    // SAFETY: an all-zero `INPUT_RECORD` is a valid value to be overwritten.
    let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
    let mut count = 0;
    // SAFETY: room for one record, and the handle is open for the call.
    if unsafe { PeekConsoleInputW(handle(&input), &mut record, 1, &mut count) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if count == 0 || u32::from(record.EventType) != KEY_EVENT {
        return Ok(None);
    }
    // SAFETY: a `KEY_EVENT` record holds a key event.
    let key = unsafe { record.Event.KeyEvent };
    // SAFETY: the console fills `UnicodeChar` for `ReadConsoleInputW` and its peek.
    let character = unsafe { key.uChar.UnicodeChar };
    let bare =
        key.bKeyDown != 0 && key.wVirtualKeyCode == 0 && (character < 0x20 || character == 0x7f);
    if !bare {
        return Ok(None);
    }
    // SAFETY: as for the peek; the record read is the one just peeked at, as only this thread
    // reads the console's input.
    if unsafe { ReadConsoleInputW(handle(&input), &mut record, 1, &mut count) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let state = key.dwControlKeyState;
    Ok(Some(BareKey {
        character: character as u8,
        alt: state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0,
        shift: state & SHIFT_PRESSED != 0,
    }))
}
