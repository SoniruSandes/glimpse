//! Recently used PDF passwords.
//! Windows: encrypted with DPAPI (only your Windows account can read them).
//! Linux: a plain file readable only by you (mode 0600), NOT encrypted. Prefer not to tick "Remember" there.

use std::{fs, path::PathBuf};

const MAX_REMEMBERED: usize = 8;

fn store_path() -> Option<PathBuf> {
    Some(crate::settings::data_dir()?.join("recent_pdf_passwords.dat"))
}

/// Most recent first.
pub fn recent() -> Vec<String> {
    let Some(path) = store_path() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|line| from_hex(line.trim()))
        .filter_map(|bytes| reveal(&bytes))
        .filter_map(|plain| String::from_utf8(plain).ok())
        .collect()
}

pub fn remember(password: &str) {
    if password.is_empty() {
        return;
    }
    let mut list = recent();
    list.retain(|p| p != password);
    list.insert(0, password.to_string());
    list.truncate(MAX_REMEMBERED);

    let Some(path) = store_path() else { return };
    let lines: Vec<String> = list
        .iter()
        .filter_map(|p| conceal(p.as_bytes()))
        .map(|bytes| to_hex(&bytes))
        .collect();
    let _ = fs::write(&path, lines.join("\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
}

pub fn forget_all() {
    if let Some(path) = store_path() {
        let _ = fs::remove_file(path);
    }
}

/// Whether "Remember" should start ticked: yes where passwords are stored encrypted.
pub fn remember_by_default() -> bool {
    cfg!(windows)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

// ---- Windows: DPAPI ----

#[cfg(windows)]
fn conceal(data: &[u8]) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{LocalFree, HLOCAL},
            Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB},
        },
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(&input, PCWSTR::null(), None, None, None, 0, &mut output).ok()?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut c_void));
        Some(bytes)
    }
}

#[cfg(windows)]
fn reveal(data: &[u8]) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    use windows::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB},
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut c_void));
        Some(bytes)
    }
}

// ---- elsewhere: plain (file mode 0600) ----

#[cfg(not(windows))]
fn conceal(data: &[u8]) -> Option<Vec<u8>> {
    Some(data.to_vec())
}

#[cfg(not(windows))]
fn reveal(data: &[u8]) -> Option<Vec<u8>> {
    Some(data.to_vec())
}
