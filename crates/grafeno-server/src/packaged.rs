//! Grafeno installed from the Microsoft Store (an MSIX package,
//! `packaging/windows/msix`).
//!
//! A packaged program cannot register the Windows service the setup
//! installs, nor write to `C:\ProgramData` with the rights it needs: from
//! the package, Grafeno runs as the user's own process (`grafeno serve`),
//! and its settings and data are the user's, in
//! `%LOCALAPPDATA%\Grafeno`. Windows redirects what a packaged program
//! writes under `%LOCALAPPDATA%` to the package's private folder,
//! `%LOCALAPPDATA%\Packages\<family>\LocalCache\Local\Grafeno`: it stays
//! through updates and goes when the package is uninstalled. A database
//! put elsewhere (`grafeno config init --db <file>`) is not redirected and
//! stays.
//!
//! Nothing changes for a program that is not packaged.

use std::path::PathBuf;

/// Whether this process runs from an MSIX package (it has a package
/// identity).
pub fn is_packaged() -> bool {
    family_name().is_some()
}

/// The package family name (`GrafenoDB.GrafenoDB_rwmas8f7748ap` from the
/// Store), when this process runs from a package.
#[cfg(windows)]
pub fn family_name() -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFamilyName;
    let mut length: u32 = 0;
    // SAFETY: a null buffer with a length of 0 only asks for the length.
    let code = unsafe { GetCurrentPackageFamilyName(&mut length, std::ptr::null_mut()) };
    if code != ERROR_INSUFFICIENT_BUFFER || length == 0 {
        // APPMODEL_ERROR_NO_PACKAGE: not packaged.
        return None;
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: the buffer holds `length` UTF-16 units, as asked.
    let code = unsafe { GetCurrentPackageFamilyName(&mut length, buffer.as_mut_ptr()) };
    if code != 0 {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

#[cfg(not(windows))]
pub fn family_name() -> Option<String> {
    None
}

/// The folder of the settings (and, in `data`, the database) of a packaged
/// Grafeno, as the program sees it.
pub fn home() -> Option<PathBuf> {
    if !is_packaged() {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("Grafeno"))
}

/// Where Windows keeps that folder on disk (what other programs and the
/// Explorer see).
pub fn home_on_disk() -> Option<PathBuf> {
    let family = family_name()?;
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("Packages")
            .join(family)
            .join("LocalCache")
            .join("Local")
            .join("Grafeno"),
    )
}

/// The refusal of what a packaged Grafeno cannot do (the service).
pub const NO_SERVICE: &str = "Grafeno from the Microsoft Store runs as your own process, not as a \
     Windows service: start it with `grafeno serve` (Ctrl+C stops it). For a service that starts \
     with Windows, install Grafeno with its setup from GitHub instead";
