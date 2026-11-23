//! Grafeno installed from the Microsoft Store (an MSIX package,
//! `packaging/windows/msix`).
//!
//! A packaged program cannot register the Windows service the setup
//! installs, nor write to `C:\ProgramData` with the rights it needs: from
//! the package, Grafeno runs as the user's own process (`grafeno serve`),
//! and its settings and data are the user's, in `%USERPROFILE%\GrafenoDB`.
//!
//! That folder is outside `%LOCALAPPDATA%` and `%APPDATA%` on purpose:
//! Windows redirects what a packaged program writes there to the package's
//! private folder, which is removed when the package is uninstalled. The
//! user's profile folder itself is not redirected, so the database stays
//! when GrafenoDB is uninstalled, as the setup's does. The first package
//! kept them in `%LOCALAPPDATA%\Grafeno` (redirected); [`move_legacy`]
//! copies them out, once.
//!
//! Nothing changes for a program that is not packaged.

use std::path::{Path, PathBuf};

use crate::config::{self, Config};

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
/// Grafeno: `%USERPROFILE%\GrafenoDB`, not redirected, kept when the
/// package is uninstalled.
pub fn home() -> Option<PathBuf> {
    if !is_packaged() {
        return None;
    }
    let profile = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(profile).join("GrafenoDB"))
}

/// Where the first package kept them, as the program sees it
/// (`%LOCALAPPDATA%\Grafeno`, which Windows redirects into the package).
pub fn legacy_home() -> Option<PathBuf> {
    if !is_packaged() {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("Grafeno"))
}

/// The settings file of a packaged Grafeno.
pub fn settings() -> Option<PathBuf> {
    home().map(|home| home.join("grafeno.toml"))
}

/// The refusal of what a packaged Grafeno cannot do (the service).
pub const NO_SERVICE: &str = "Grafeno from the Microsoft Store runs as your own process, not as a \
     Windows service: start it with `grafeno serve` (Ctrl+C stops it). For a service that starts \
     with Windows, install Grafeno with its setup from GitHub instead";

/// The note left in the old folder once its data was copied out.
const MOVED: &str = "MOVED-TO-GRAFENODB-FOLDER.txt";

/// What [`move_legacy`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// There was nothing to move, or it was moved before.
    Nothing,
    /// The settings (and the database) were copied to `home`.
    Copied { from: PathBuf, to: PathBuf },
}

/// Copies the first package's settings and database out of the package's
/// private folder to `home`, once: only when `home` has no settings yet
/// and the old folder has some. The database goes through a consistent
/// backup and is checked as it is restored; the settings keep their token
/// and everything else, with the new database path. The old folder is
/// left as it was, with a note saying where its contents went. Nothing is
/// replaced: an existing file at the destination stops it.
pub fn move_legacy(legacy: &Path, home: &Path) -> Result<Moved, String> {
    let old_settings = legacy.join("grafeno.toml");
    let new_settings = home.join("grafeno.toml");
    if new_settings.exists() || !old_settings.exists() || legacy.join(MOVED).exists() {
        return Ok(Moved::Nothing);
    }
    let mut settings = Config::load(&old_settings)
        .map_err(|error| format!("cannot read {}: {error}", old_settings.display()))?;
    if let Some(old_db) = settings.db.clone().filter(|db| db.exists()) {
        let new_db = home.join("data").join(
            old_db
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| "grafeno.redb".into()),
        );
        if new_db.exists() {
            return Err(format!(
                "{} exists already: move it away, or copy {} yourself",
                new_db.display(),
                old_db.display()
            ));
        }
        copy_database(&old_db, &new_db)?;
        settings.db = Some(new_db);
    }
    config::write_new(&new_settings, settings, false)?;
    let _ = std::fs::write(
        legacy.join(MOVED),
        format!(
            "Grafeno copied these settings and this database to {} (where they stay when \
             GrafenoDB is uninstalled). This folder was left as it was; it goes when the \
             package is uninstalled.\n",
            home.display()
        ),
    );
    Ok(Moved::Copied {
        from: legacy.to_path_buf(),
        to: home.to_path_buf(),
    })
}

/// A consistent copy of the database at `from` to the new file `to`: a
/// backup, then a restore that checks every page and namespace. `from` must
/// not be open in another process.
pub fn copy_database(from: &Path, to: &Path) -> Result<(), String> {
    let parent = to.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temporary = parent.join(".grafeno-copy.redb");
    let _ = std::fs::remove_file(&temporary);
    let source = grafeno::Grafeno::open(from).map_err(|error| {
        if error.to_string().contains("already open") {
            format!(
                "{} is open in another process (a running grafeno serve?): stop it and try again",
                from.display()
            )
        } else {
            format!("cannot open {}: {error}", from.display())
        }
    })?;
    let backup = source
        .begin_read()
        .and_then(|read| read.backup(&temporary))
        .map_err(|error| format!("cannot back up {}: {error}", from.display()));
    drop(source);
    if let Err(error) = backup {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    let restored = grafeno::Grafeno::recover_backup(&temporary, to)
        .map(|_| ())
        .map_err(|error| format!("the copy of {} did not check out: {error}", from.display()));
    let _ = std::fs::remove_file(&temporary);
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("grafeno-{name}-{}", grafeno::NodeId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The first package's settings and database come out once, whole,
    /// with the same token; the old folder stays as it was.
    #[test]
    fn the_first_packages_files_are_copied_out_once() {
        let legacy = folder("legacy");
        let home = folder("home").join("GrafenoDB");
        let old_db = legacy.join("data").join("grafeno.redb");
        let old = config::init(
            &legacy.join("grafeno.toml"),
            &old_db,
            "127.0.0.1:7810".parse().unwrap(),
            false,
        )
        .unwrap();
        {
            let db = grafeno::Grafeno::open(&old_db).unwrap();
            db.write(|tx| {
                tx.create_namespace("notes", None)?;
                tx.create_node("notes", grafeno::NewNode::new("Note"))
            })
            .unwrap();
        }

        let moved = move_legacy(&legacy, &home).unwrap();
        assert_eq!(
            moved,
            Moved::Copied {
                from: legacy.clone(),
                to: home.clone()
            }
        );
        let new = Config::load(&home.join("grafeno.toml")).unwrap();
        assert_eq!(new.token, old.token);
        let new_db = home.join("data").join("grafeno.redb");
        assert_eq!(new.db.as_deref(), Some(new_db.as_path()));
        let copy = grafeno::Grafeno::open(&new_db).unwrap();
        let read = copy.begin_read().unwrap();
        assert_eq!(read.stats("notes").unwrap().nodes, 1);
        drop(read);
        drop(copy);
        // The old folder is untouched, and says where its contents went.
        assert!(old_db.exists() && legacy.join("grafeno.toml").exists());
        assert!(legacy.join(MOVED).exists());
        // Once only.
        assert_eq!(move_legacy(&legacy, &home).unwrap(), Moved::Nothing);
        let _ = std::fs::remove_dir_all(&legacy);
        let _ = std::fs::remove_dir_all(home.parent().unwrap());
    }

    /// Nothing at the destination is ever replaced.
    #[test]
    fn a_database_at_the_destination_stops_the_copy() {
        let legacy = folder("legacy");
        let home = folder("home");
        config::init(
            &legacy.join("grafeno.toml"),
            &legacy.join("data").join("grafeno.redb"),
            "127.0.0.1:7810".parse().unwrap(),
            false,
        )
        .unwrap();
        drop(grafeno::Grafeno::open(legacy.join("data").join("grafeno.redb")).unwrap());
        std::fs::create_dir_all(home.join("data")).unwrap();
        std::fs::write(home.join("data").join("grafeno.redb"), b"mine").unwrap();
        assert!(move_legacy(&legacy, &home).is_err());
        assert_eq!(
            std::fs::read(home.join("data").join("grafeno.redb")).unwrap(),
            b"mine"
        );
        assert!(!home.join("grafeno.toml").exists());
        assert!(!legacy.join(MOVED).exists());
        let _ = std::fs::remove_dir_all(&legacy);
        let _ = std::fs::remove_dir_all(&home);
    }
}
