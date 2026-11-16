//! Where the config file lives, and how it is read and written.

use crate::model::{LimitsConfig, Provider, ProviderConfig};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The user's home directory.
///
/// `HOME` is checked before `USERPROFILE` on every platform so a test (or a
/// caller running a probe CLI in a sandbox) can redirect credential and config
/// lookups by setting one variable.
pub fn home_dir() -> PathBuf {
    for var in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(var)
            && !value.is_empty()
        {
            return PathBuf::from(value);
        }
    }
    PathBuf::from(".")
}

/// The config file this process will read and write.
///
/// `LIMITS_CONFIG` wins outright. Otherwise the XDG location is used.
pub fn config_path() -> PathBuf {
    if let Some(value) = std::env::var_os("LIMITS_CONFIG")
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }

    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));

    base.join("limits").join("config.json")
}

/// Read the config, writing a default one first if none exists.
///
/// Never fails: a machine with an unreadable or corrupt config still needs to
/// report quotas, so a bad file falls back to defaults rather than aborting.
pub fn load() -> LimitsConfig {
    let path = config_path();
    let Ok(data) = std::fs::read_to_string(&path) else {
        let fresh = LimitsConfig::default();
        let _ = save(&fresh);
        return fresh;
    };

    match serde_json::from_str::<LimitsConfig>(&data) {
        Ok(config) if !config.providers.is_empty() => migrate(config),
        _ => LimitsConfig::default(),
    }
}

/// Append providers this build knows about that the file predates.
///
/// New entries arrive enabled, matching a fresh config: a provider that reads a
/// credential the user already has should start reporting without being
/// switched on by hand.
fn migrate(mut config: LimitsConfig) -> LimitsConfig {
    for provider in Provider::ALL {
        if config.get(provider.id()).is_none() {
            config
                .providers
                .push(ProviderConfig::new(provider.id(), true));
        }
    }
    config
}

/// Write the config, creating its directory if needed.
pub fn save(config: &LimitsConfig) -> io::Result<()> {
    save_to(&config_path(), config)
}

/// Write a config to an explicit path.
///
/// Restricts permissions on Unix to owner-only (`0o600`) because the file
/// stores API keys and session credentials in plain text.
pub fn save_to(path: &Path, config: &LimitsConfig) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            #[allow(clippy::collapsible_if)]
            if let Err(e) = builder.create(parent) {
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(parent)?;
        }
    }
    let data = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    use std::io::Write;
    write_atomically(path, |file| file.write_all(data.as_bytes()))
}

fn write_atomically(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (temp_path, mut file) = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".limits-config-{}-{sequence}.tmp",
            std::process::id()
        ));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&temp_path) {
            Ok(file) => break (temp_path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result?;

    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};

    let from: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: Both paths are NUL-terminated UTF-16 strings that remain alive
    // for the duration of the call.
    let replaced = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_adds_providers_the_file_predates() {
        let old = LimitsConfig {
            version: 1,
            providers: vec![ProviderConfig::new("claude", true)],
        };
        let migrated = migrate(old);

        assert!(migrated.get("opencode").is_some());
        assert!(migrated.get("copilot").is_some());
        assert!(migrated.get("grok").is_some());
        assert_eq!(migrated.providers.len(), Provider::ALL.len());
    }

    #[test]
    fn migration_leaves_existing_entries_untouched() {
        let mut disabled = ProviderConfig::new("claude", false);
        disabled.api_key = "secret".into();
        let migrated = migrate(LimitsConfig {
            version: 1,
            providers: vec![disabled],
        });

        let claude = migrated.get("claude").unwrap();
        assert!(!claude.is_enabled());
        assert_eq!(claude.api_key, "secret");
        assert_eq!(migrated.providers[0].id, "claude");
    }

    #[test]
    fn config_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");

        let mut config = LimitsConfig::default();
        config.get_mut("Grok").unwrap().api_key = "xai-test".into();
        save_to(&path, &config).unwrap();
        config.get_mut("Grok").unwrap().api_key = "updated-test".into();
        save_to(&path, &config).unwrap();

        let reloaded: LimitsConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.get("grok").unwrap().api_key, "updated-test");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "temporary config file should be renamed away"
        );
    }

    #[test]
    fn a_failed_write_preserves_the_previous_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "original config").unwrap();

        let result = write_atomically(&path, |file| {
            use std::io::Write;
            file.write_all(b"partial config")?;
            Err(io::Error::other("simulated write failure"))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original config");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn config_file_and_directory_have_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("nested");
        let path = parent.join("config.json");

        let config = LimitsConfig::default();
        save_to(&path, &config).unwrap();

        let dir_metadata = std::fs::metadata(&parent).unwrap();
        assert_eq!(dir_metadata.permissions().mode() & 0o777, 0o700);

        let file_metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(file_metadata.permissions().mode() & 0o777, 0o600);
    }
}
