//! String values under `HKEY_CURRENT_USER`, read and written through the registry API.

use std::io;

use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR},
    System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    },
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

/// A key this module opened, closed when dropped.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key handle this module opened and nothing else closes.
        unsafe { RegCloseKey(self.0) };
    }
}

/// The string value `name` of the key `key` under `HKEY_CURRENT_USER`, `None` when the key or
/// the value does not exist.
pub fn read_string(key: &str, name: &str) -> io::Result<Option<String>> {
    let key = wide(key);
    let name = wide(name);
    let mut size = 0u32;
    loop {
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        let data = if buffer.is_empty() {
            std::ptr::null_mut()
        } else {
            buffer.as_mut_ptr().cast()
        };
        // SAFETY: NUL-terminated names, and a buffer of `size` bytes (or none, to ask the size).
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data,
                &mut size,
            )
        };
        match status {
            ERROR_FILE_NOT_FOUND => return Ok(None),
            ERROR_SUCCESS if size == 0 => return Ok(Some(String::new())),
            // Asked for the size only: read it now that `size` holds it.
            ERROR_SUCCESS if buffer.is_empty() => continue,
            ERROR_SUCCESS => {
                let len = (size as usize / 2).min(buffer.len());
                let text = &buffer[..len];
                let text = text.strip_suffix(&[0]).unwrap_or(text);
                return Ok(Some(String::from_utf16_lossy(text)));
            }
            // The value grew since its size was read: try again with the new size.
            ERROR_MORE_DATA => continue,
            other => return Err(io::Error::from_raw_os_error(other as i32)),
        }
    }
}

/// Sets the string value `name` of the key `key` under `HKEY_CURRENT_USER`, creating the key.
pub fn write_string(key: &str, name: &str, value: &str) -> io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    let value = wide(value);
    let mut handle: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated key name and a pointer for the handle; the rest is optional.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut handle,
            std::ptr::null_mut(),
        )
    })?;
    let handle = Key(handle);
    let bytes = u32::try_from(value.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the value is too long"))?;
    // SAFETY: an open key, a NUL-terminated name and `bytes` bytes of a NUL-terminated string.
    check(unsafe {
        RegSetValueExW(
            handle.0,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            bytes,
        )
    })
}

/// Removes the value `name` of the key `key` under `HKEY_CURRENT_USER`; a missing key or value
/// is not an error.
pub fn delete_value(key: &str, name: &str) -> io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    let mut handle: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated key name and a pointer for the handle.
    match unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut handle,
        )
    } {
        ERROR_FILE_NOT_FOUND => return Ok(()),
        status => check(status)?,
    }
    let handle = Key(handle);
    // SAFETY: an open key and a NUL-terminated name.
    match unsafe { RegDeleteValueW(handle.0, name.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(()),
        status => check(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_reads_and_deletes_a_value() {
        let key = format!(r"Software\Portico-test-{}\registry", std::process::id());
        assert_eq!(read_string(&key, "v").unwrap(), None);
        write_string(&key, "v", r#""C:\A B\x.exe" --portico "C:\p.exe""#).unwrap();
        assert_eq!(
            read_string(&key, "v").unwrap().as_deref(),
            Some(r#""C:\A B\x.exe" --portico "C:\p.exe""#)
        );
        delete_value(&key, "v").unwrap();
        delete_value(&key, "v").unwrap();
        assert_eq!(read_string(&key, "v").unwrap(), None);
        let parent = wide(&format!(r"Software\Portico-test-{}", std::process::id()));
        // SAFETY: a NUL-terminated key name. Deleting its tree leaves the key itself.
        unsafe {
            windows_sys::Win32::System::Registry::RegDeleteTreeW(
                HKEY_CURRENT_USER,
                parent.as_ptr(),
            );
            windows_sys::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, parent.as_ptr());
        }
    }
}
