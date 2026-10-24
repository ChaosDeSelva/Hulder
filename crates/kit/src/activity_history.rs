use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const FILE_BYTES: u64 = 1024 * 1024;
const HISTORY_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    pub observed_at: String,
    pub path: PathBuf,
    pub kind: String,
    pub fields: Vec<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct History {
    files: BTreeMap<PathBuf, Fingerprint>,
    changes: Vec<Change>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Fingerprint {
    state: String,
    digest: String,
    fields: BTreeMap<String, String>,
}

pub fn workspace_file(directory: &Path, roots: &[PathBuf]) -> PathBuf {
    let mut roots: Vec<_> = roots
        .iter()
        .map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    roots.sort();
    roots.dedup();
    let mut hasher = Sha256::new();
    for root in roots {
        let bytes = root.as_os_str().as_encoded_bytes();
        hasher.update(bytes.len().to_le_bytes());
        hasher.update(bytes);
    }
    directory.join(format!("{:x}.json", hasher.finalize()))
}
fn fingerprint(path: &Path) -> io::Result<Fingerprint> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Fingerprint {
                state: "Missing".into(),
                digest: String::new(),
                fields: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > FILE_BYTES {
        return Err(io::Error::other(
            "Configuration is not a bounded regular file",
        ));
    }
    let mut text = String::new();
    fs::File::open(path)?
        .take(FILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > FILE_BYTES {
        return Err(io::Error::other("Configuration exceeded size limit"));
    }
    let value = if path
        .extension()
        .is_some_and(|extension| extension == "toml")
    {
        toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|value| serde_json::to_value(value).ok())
    } else {
        serde_json::from_str::<serde_json::Value>(&text).ok()
    };
    let mut fields = BTreeMap::new();
    fn walk(
        value: &serde_json::Value,
        pointer: &str,
        fields: &mut BTreeMap<String, String>,
        depth: usize,
    ) {
        if fields.len() >= 128 || depth > 16 || pointer.len() > 256 {
            return;
        }
        match value {
            serde_json::Value::Object(object) if !object.is_empty() => {
                for (key, value) in object {
                    let key: String = key
                        .replace('~', "~0")
                        .replace('/', "~1")
                        .chars()
                        .take(128)
                        .collect();
                    walk(value, &format!("{pointer}/{key}"), fields, depth + 1);
                }
            }
            serde_json::Value::Array(array) if !array.is_empty() => {
                for (index, value) in array.iter().take(128).enumerate() {
                    walk(value, &format!("{pointer}/{index}"), fields, depth + 1);
                }
            }
            _ => {
                fields.insert(
                    pointer.to_string(),
                    format!("{:x}", Sha256::digest(value.to_string().as_bytes())),
                );
            }
        }
    }
    if let Some(value) = &value {
        walk(value, "", &mut fields, 0);
    }
    Ok(Fingerprint {
        state: if value.is_some() {
            "Parsed"
        } else {
            "Invalid configuration"
        }
        .into(),
        digest: format!("{:x}", Sha256::digest(text.as_bytes())),
        fields,
    })
}

pub fn observe(
    history_path: &Path,
    paths: &[PathBuf],
    now: &str,
) -> io::Result<(Vec<Change>, Vec<String>)> {
    let directory = history_path
        .parent()
        .ok_or_else(|| io::Error::other("Missing history directory"))?;
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    let lock_path = history_path.with_extension("lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(io::Error::other("Invalid history lock"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(lock_path)?;
    lock.try_lock().map_err(io::Error::other)?;
    let mut history: History = match fs::symlink_metadata(history_path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() > HISTORY_BYTES {
                return Err(io::Error::other("Invalid history file"));
            }
            let mut text = String::new();
            fs::File::open(history_path)?
                .take(HISTORY_BYTES + 1)
                .read_to_string(&mut text)?;
            if text.len() as u64 > HISTORY_BYTES {
                return Err(io::Error::other("History exceeded size limit"));
            }
            serde_json::from_str(&text).map_err(io::Error::other)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => History::default(),
        Err(error) => return Err(error),
    };
    let mut notices = Vec::new();
    let mut candidates: BTreeSet<_> = history.files.keys().cloned().collect();
    candidates.extend(paths.iter().cloned());
    if candidates.len() > 256 {
        notices.push("Configuration history tracks at most 256 source files".into());
    }
    let mut dirty = false;
    for path in candidates.into_iter().take(256) {
        let current = match fingerprint(&path) {
            Ok(current) => current,
            Err(_) => {
                notices.push(format!(
                    "Configuration history could not inspect {}",
                    path.display()
                ));
                continue;
            }
        };
        let previous = history.files.get(&path);
        if previous == Some(&current) {
            continue;
        }
        let kind = if previous.is_none() {
            "Baseline"
        } else if current.state == "Missing" {
            "Removed"
        } else if previous.is_some_and(|entry| entry.state == "Missing") {
            "Added"
        } else if current.state == "Invalid configuration" {
            "Invalid configuration"
        } else {
            "Changed"
        };
        let mut fields = Vec::new();
        if let Some(previous) = previous {
            let keys: BTreeSet<_> = previous
                .fields
                .keys()
                .chain(current.fields.keys())
                .collect();
            for key in keys {
                if previous.fields.get(key) != current.fields.get(key) {
                    fields.push(key.clone());
                }
            }
            if fields.is_empty() && previous.digest != current.digest {
                fields.push(
                    "Formatting, invalid content or fields outside the bounded detail inventory"
                        .into(),
                );
            }
            if fields.len() > 50 {
                fields.truncate(50);
                fields.push("Additional changed fields omitted".into());
            }
        }
        history.changes.push(Change {
            observed_at: now.into(),
            path: path.clone(),
            kind: kind.into(),
            fields,
        });
        history.files.insert(path, current);
        dirty = true;
    }
    if history.changes.len() > 200 {
        history.changes.drain(..history.changes.len() - 200);
    }
    if dirty {
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temporary, &history).map_err(io::Error::other)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(history_path)
            .map_err(|error| error.error)?;
    }
    history.changes.reverse();
    Ok((history.changes, notices))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persists_changes_without_values_and_detects_removal() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("settings.json");
        let history = directory.path().join("history/changes.json");
        fs::write(
            &config,
            r#"{"hooks":{"command":"secret command value"},"token":"supersecret"}"#,
        )
        .unwrap();
        assert_eq!(
            observe(&history, &[config.clone()], "1").unwrap().0[0].kind,
            "Baseline"
        );
        assert_eq!(
            observe(&history, &[config.clone()], "2").unwrap().0.len(),
            1
        );
        fs::write(
            &config,
            r#"{"hooks":{"command":"changed secret"},"token":"supersecret"}"#,
        )
        .unwrap();
        let changes = observe(&history, &[config.clone()], "3").unwrap().0;
        assert_eq!(changes[0].fields, vec!["/hooks/command"]);
        let saved = fs::read_to_string(&history).unwrap();
        assert!(!saved.contains("supersecret"));
        assert!(!saved.contains("secret command value"));
        assert!(!saved.contains("changed secret"));
        fs::remove_file(&config).unwrap();
        assert_eq!(observe(&history, &[], "4").unwrap().0[0].kind, "Removed");
        fs::write(&config, "{}").unwrap();
        assert_eq!(observe(&history, &[], "5").unwrap().0[0].kind, "Added");
    }
    #[test]
    fn corrupt_history_is_not_overwritten_and_workspace_key_is_stable() {
        let directory = tempfile::tempdir().unwrap();
        let history = directory.path().join("history.json");
        fs::write(&history, "broken").unwrap();
        assert!(observe(&history, &[], "1").is_err());
        assert_eq!(fs::read_to_string(&history).unwrap(), "broken");
        assert_eq!(
            workspace_file(directory.path(), &["a".into(), "b".into()]),
            workspace_file(directory.path(), &["b".into(), "a".into()])
        );
    }
    #[test]
    fn unreadable_source_does_not_report_removal() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("settings.json");
        let history = directory.path().join("history/changes.json");
        fs::write(&config, "{}").unwrap();
        observe(&history, &[config.clone()], "1").unwrap();
        fs::remove_file(&config).unwrap();
        fs::create_dir(&config).unwrap();
        let (changes, notices) = observe(&history, &[], "2").unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(notices.len(), 1);
    }
}
