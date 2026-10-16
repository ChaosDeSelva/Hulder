//! Where profiles live: metadata in `profiles.json` under the config
//! directory, secrets behind [`SecretStore`]. The only real implementation is
//! the platform Keychain through gpui (local Keychain on macOS, never a cloud
//! provider); tests use [`FakeSecretStore`].

use crate::model::{KEYCHAIN_SERVICE, ProfilesFile, Secret};
use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Task};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const PROFILES_FILE: &str = "profiles.json";

pub trait SecretStore: Send + Sync {
    fn read(&self, profile: &str, cx: &mut App) -> Task<Result<Option<Secret>>>;
    fn write(&self, profile: &str, secret: &Secret, cx: &mut App) -> Task<Result<()>>;
    fn delete(&self, profile: &str, cx: &mut App) -> Task<Result<()>>;
}

/// The Keychain item for a profile. gpui keys items by "server", so each
/// profile needs a distinct one; the account is the profile name.
pub fn keychain_server(profile: &str) -> String {
    format!("{KEYCHAIN_SERVICE}/{profile}")
}

pub struct KeychainSecretStore;

impl SecretStore for KeychainSecretStore {
    fn read(&self, profile: &str, cx: &mut App) -> Task<Result<Option<Secret>>> {
        let read = cx.read_credentials(&keychain_server(profile));
        cx.background_spawn(async move {
            match read.await? {
                Some((_account, bytes)) => Ok(Some(Secret::from_bytes(&bytes)?)),
                None => Ok(None),
            }
        })
    }

    fn write(&self, profile: &str, secret: &Secret, cx: &mut App) -> Task<Result<()>> {
        let bytes = match secret.to_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.write_credentials(&keychain_server(profile), profile, &bytes)
    }

    fn delete(&self, profile: &str, cx: &mut App) -> Task<Result<()>> {
        cx.delete_credentials(&keychain_server(profile))
    }
}

#[derive(Default)]
pub struct FakeSecretStore {
    secrets: Mutex<HashMap<String, Secret>>,
}

impl FakeSecretStore {
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.secrets.lock().keys().cloned().collect();
        names.sort();
        names
    }
}

impl SecretStore for FakeSecretStore {
    fn read(&self, profile: &str, _cx: &mut App) -> Task<Result<Option<Secret>>> {
        Task::ready(Ok(self.secrets.lock().get(profile).cloned()))
    }

    fn write(&self, profile: &str, secret: &Secret, _cx: &mut App) -> Task<Result<()>> {
        self.secrets
            .lock()
            .insert(profile.to_string(), secret.clone());
        Task::ready(Ok(()))
    }

    fn delete(&self, profile: &str, _cx: &mut App) -> Task<Result<()>> {
        self.secrets.lock().remove(profile);
        Task::ready(Ok(()))
    }
}

pub fn default_profiles_path() -> PathBuf {
    paths::config_dir().join("hoard").join(PROFILES_FILE)
}

/// A missing file is an empty Hoard; a symlink is refused rather than followed.
pub fn load_profiles(path: &Path) -> Result<ProfilesFile> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("{} is a symlink; refusing to read it", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProfilesFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Writes through a randomly named 0600 temp file (`O_EXCL`, so no planted
/// link can be followed) in the 0700 directory, syncs it, re-checks that the
/// target is not a symlink, then renames it over the target.
pub fn save_profiles(path: &Path, file: &ProfilesFile) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let directory = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    if std::fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        anyhow::bail!("{} is a symlink; refusing to use it", directory.display());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let text = serde_json::to_string_pretty(file).context("serializing profiles")?;
    let mut temp = tempfile::Builder::new()
        .prefix(".profiles-")
        .suffix(".tmp")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(directory)
        .with_context(|| format!("creating a temp file in {}", directory.display()))?;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("writing {}", temp.path().display()))?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        anyhow::bail!("{} is a symlink; refusing to replace it", path.display());
    }
    temp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Profile, ProfileKind};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn profiles_file_is_private_and_missing_means_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("hoard").join(PROFILES_FILE);
        assert_eq!(load_profiles(&path).expect("missing"), ProfilesFile::default());

        let file = ProfilesFile {
            profiles: vec![Profile {
                name: "work".into(),
                kind: ProfileKind::Bearer,
                hosts: vec!["api.test".into()],
                created: "2026-10-13".into(),
                header_name: None,
                keep_session_cookies: false,
            }],
        };
        save_profiles(&path, &file).expect("save");
        assert_eq!(load_profiles(&path).expect("load"), file);
        let directory_mode = std::fs::metadata(temp.path().join("hoard"))
            .expect("dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        let file_mode = std::fs::metadata(&path).expect("file").permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(temp.path().join("hoard"))
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");

        std::fs::remove_file(&path).expect("remove");
        let elsewhere = temp.path().join("elsewhere.json");
        std::fs::write(&elsewhere, "keep").expect("write");
        std::os::unix::fs::symlink(&elsewhere, &path).expect("symlink");
        assert!(load_profiles(&path).is_err(), "symlink must be refused");
        assert!(save_profiles(&path, &file).is_err(), "symlinked target refused");
        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("read"),
            "keep",
            "link target untouched"
        );
        let leftovers: Vec<_> = std::fs::read_dir(temp.path().join("hoard"))
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind after refusal");
    }

    #[test]
    fn keychain_server_is_per_profile() {
        assert_eq!(keychain_server("work"), "Hulder Hoard/work");
        assert_ne!(keychain_server("a"), keychain_server("b"));
    }
}
