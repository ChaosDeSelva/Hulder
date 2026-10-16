use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

const MAX_MANIFEST: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub panels: Vec<Panel>,
    #[serde(default)]
    pub templates: Vec<Template>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Process,
    CreateProject,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: String,
    pub label: String,
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub id: String,
    pub label: String,
    pub directory: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub manifest: Manifest,
    pub enabled: bool,
    pub settings: BTreeMap<String, String>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn relative_file(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && !value.contains(':')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST,
            "Add on manifest exceeds 1 MiB"
        );
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == 1,
            "Unsupported Hulder add on API version"
        );
        ensure!(identifier(&self.id), "Invalid add on identifier");
        semver::Version::parse(&self.version).context("Invalid add on version")?;
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 120,
            "Invalid add on name"
        );
        ensure!(
            self.commands.len() <= 32 && self.panels.len() <= 16 && self.templates.len() <= 16,
            "Too many add on contributions"
        );
        ensure!(
            self.settings.keys().all(|key| identifier(key)),
            "Invalid setting identifier"
        );
        let mut command_ids = std::collections::BTreeSet::new();
        for command in &self.commands {
            ensure!(
                identifier(&command.id) && command_ids.insert(&command.id),
                "Invalid or duplicate command identifier"
            );
            ensure!(
                self.permissions.contains(&Permission::Process),
                "Commands require process permission"
            );
            ensure!(
                !command.executable.is_empty() && !command.executable.contains('\0'),
                "Invalid executable"
            );
            for argument in &command.args {
                ensure!(!argument.contains('\0'), "Invalid command argument");
            }
            command.resolve(&self.settings)?;
        }
        for panel in &self.panels {
            ensure!(
                panel.commands.iter().all(|id| command_ids.contains(id)),
                "Panel references an unknown command"
            );
        }
        let mut template_ids = std::collections::BTreeSet::new();
        for template in &self.templates {
            ensure!(
                self.permissions.contains(&Permission::CreateProject),
                "Templates require create_project permission"
            );
            ensure!(
                identifier(&template.id) && template_ids.insert(&template.id),
                "Invalid or duplicate template identifier"
            );
            ensure!(
                identifier(&template.directory),
                "Invalid template directory"
            );
            ensure!(
                !template.files.is_empty() && template.files.len() <= 128,
                "Invalid template file count"
            );
            for name in template.files.keys() {
                ensure!(relative_file(name), "Unsafe template path");
                ensure!(
                    !name.split('/').any(|part| part == ".git"),
                    "Templates cannot contain Git metadata"
                );
                ensure!(
                    !template
                        .files
                        .keys()
                        .any(|other| other != name && other.starts_with(&format!("{name}/"))),
                    "Template file conflicts with directory"
                );
            }
        }
        Ok(())
    }
}

impl Command {
    pub fn resolve(&self, settings: &BTreeMap<String, String>) -> Result<(String, Vec<String>)> {
        fn resolve(value: &str, settings: &BTreeMap<String, String>) -> Result<String> {
            if let Some(key) = value
                .strip_prefix("${settings.")
                .and_then(|value| value.strip_suffix('}'))
            {
                let value = settings.get(key).context("Unknown add on setting")?;
                ensure!(!value.contains('\0'), "Invalid setting value");
                Ok(value.clone())
            } else {
                ensure!(
                    !value.contains("${"),
                    "Settings must occupy a complete argument"
                );
                Ok(value.to_owned())
            }
        }
        let executable = resolve(&self.executable, settings)?;
        ensure!(
            !executable.trim().is_empty(),
            "Executable must not be empty"
        );
        Ok((
            executable,
            self.args
                .iter()
                .map(|value| resolve(value, settings))
                .collect::<Result<_>>()?,
        ))
    }
}

pub fn read_manifest(directory: &Path) -> Result<Manifest> {
    let path = directory.join("hulder-addon.json");
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Manifest must be a regular file"
    );
    ensure!(
        fs::metadata(&path)?.len() <= MAX_MANIFEST,
        "Add on manifest exceeds 1 MiB"
    );
    Manifest::parse(&fs::read(path)?)
}

pub fn load(store: &Path) -> Result<Vec<Installed>> {
    if !store.exists() {
        return Ok(Vec::new());
    }
    let mut installed = Vec::new();
    for entry in fs::read_dir(store)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        ensure!(
            fs::symlink_metadata(&path)?.is_file(),
            "Add on state must be a regular file"
        );
        ensure!(
            fs::metadata(&path)?.len() <= MAX_MANIFEST * 2,
            "Add on state exceeds size limit"
        );
        let entry: Installed = serde_json::from_slice(&fs::read(&path)?)?;
        entry.manifest.validate()?;
        ensure!(
            path.file_stem().and_then(|value| value.to_str()) == Some(&entry.manifest.id),
            "Add on state identifier mismatch"
        );
        ensure!(
            entry.settings.keys().eq(entry.manifest.settings.keys()),
            "Unknown or missing add on settings"
        );
        for command in &entry.manifest.commands {
            command.resolve(&entry.settings)?;
        }
        installed.push(entry);
    }
    installed.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
    Ok(installed)
}

pub fn save(store: &Path, installed: &Installed) -> Result<()> {
    installed.manifest.validate()?;
    ensure!(
        installed
            .settings
            .keys()
            .eq(installed.manifest.settings.keys()),
        "Unknown or missing settings"
    );
    for command in &installed.manifest.commands {
        command.resolve(&installed.settings)?;
    }
    fs::create_dir_all(store)?;
    let mut temporary = tempfile::NamedTempFile::new_in(store)?;
    let bytes = serde_json::to_vec_pretty(installed)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST * 2,
        "Add on state exceeds size limit"
    );
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(store.join(format!("{}.json", installed.manifest.id)))?;
    Ok(())
}

pub fn install(store: &Path, manifest: Manifest) -> Result<()> {
    manifest.validate()?;
    let previous = load(store)?
        .into_iter()
        .find(|entry| entry.manifest.id == manifest.id);
    let mut settings = manifest.settings.clone();
    if let Some(previous) = previous {
        for (key, value) in previous.settings {
            if settings.contains_key(&key) {
                settings.insert(key, value);
            }
        }
    }
    // An update is a new trust decision, including when its version is unchanged.
    save(
        store,
        &Installed {
            manifest,
            enabled: false,
            settings,
        },
    )
}

pub fn remove(store: &Path, id: &str) -> Result<()> {
    ensure!(identifier(id), "Invalid add on identifier");
    fs::remove_file(store.join(format!("{id}.json")))?;
    Ok(())
}

pub fn verify_approval(current: &Installed, approved: &Installed) -> Result<()> {
    ensure!(
        current.enabled && serde_json::to_vec(current)? == serde_json::to_vec(approved)?,
        "Add on changed during approval; run it again"
    );
    Ok(())
}

pub fn create_project(parent: &Path, template: &Template) -> Result<PathBuf> {
    ensure!(
        identifier(&template.directory),
        "Invalid template directory"
    );
    ensure!(
        template
            .files
            .keys()
            .all(|name| relative_file(name) && !name.split('/').any(|part| part == ".git")),
        "Unsafe template path"
    );
    let destination = parent.canonicalize()?.join(&template.directory);
    if destination.symlink_metadata().is_ok() {
        bail!("Destination already exists; no files were changed");
    }
    let temporary = tempfile::Builder::new()
        .prefix(".hulder-template-")
        .tempdir_in(parent)?;
    for (name, content) in &template.files {
        let path = temporary.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(content.as_bytes())?;
    }
    // Reserve the destination rather than renaming over a folder created concurrently.
    fs::create_dir(&destination)?;
    for entry in fs::read_dir(temporary.path())? {
        let entry = entry?;
        fs::rename(entry.path(), destination.join(entry.file_name()))?;
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        Manifest {
            api_version: 1,
            id: "love".into(),
            name: "Love".into(),
            version: "1.0.0".into(),
            description: "Game tools".into(),
            permissions: vec![Permission::Process, Permission::CreateProject],
            settings: BTreeMap::from([("runtime".into(), "love".into())]),
            commands: vec![Command {
                id: "run".into(),
                label: "Run".into(),
                executable: "${settings.runtime}".into(),
                args: vec![".".into()],
            }],
            panels: vec![Panel {
                title: "Game".into(),
                text: "Run a game".into(),
                commands: vec!["run".into()],
            }],
            templates: vec![Template {
                id: "game".into(),
                label: "Game".into(),
                directory: "game".into(),
                files: BTreeMap::from([("main.lua".into(), "function love.draw() end".into())]),
            }],
        }
    }
    #[test]
    fn approval_is_invalid_after_disable_or_update() -> Result<()> {
        let approved = Installed {
            manifest: manifest(),
            enabled: true,
            settings: manifest().settings,
        };
        let mut current = approved.clone();
        verify_approval(&current, &approved)?;
        current.enabled = false;
        assert!(verify_approval(&current, &approved).is_err());
        current.enabled = true;
        current.manifest.version = "1.0.1".into();
        assert!(verify_approval(&current, &approved).is_err());
        current = approved.clone();
        current.settings.insert("runtime".into(), "other".into());
        assert!(verify_approval(&current, &approved).is_err());
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_manifests_and_existing_symlink_destinations() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(
            outside.path().join("manifest.json"),
            serde_json::to_vec(&manifest())?,
        )?;
        std::os::unix::fs::symlink(
            outside.path().join("manifest.json"),
            directory.path().join("hulder-addon.json"),
        )?;
        assert!(read_manifest(directory.path()).is_err());
        std::os::unix::fs::symlink(outside.path(), directory.path().join("game"))?;
        let manifest = manifest();
        assert!(
            create_project(
                directory.path(),
                manifest.templates.first().context("missing")?
            )
            .is_err()
        );
        assert!(!outside.path().join("main.lua").exists());
        Ok(())
    }
    #[test]
    fn reference_addon_validates_and_creates_playable_project() -> Result<()> {
        let manifest = Manifest::parse(include_bytes!(
            "../../../examples/addons/love/hulder-addon.json"
        ))?;
        let parent = tempfile::tempdir()?;
        let template = manifest.templates.first().context("missing template")?;
        let path = create_project(parent.path(), template)?;
        assert!(fs::read_to_string(path.join("main.lua"))?.contains("function love.draw()"));
        Ok(())
    }
    #[test]
    fn rejects_future_api_and_unknown_permissions() -> Result<()> {
        let mut manifest = manifest();
        manifest.api_version = 2;
        assert!(manifest.validate().is_err());
        let mut value = serde_json::to_value(manifest)?;
        value["api_version"] = 1.into();
        value["permissions"] = serde_json::json!(["network"]);
        assert!(Manifest::parse(&serde_json::to_vec(&value)?).is_err());
        Ok(())
    }
    #[test]
    fn rejects_undeclared_and_duplicate_commands() -> Result<()> {
        let mut manifest = manifest();
        manifest.permissions.clear();
        assert!(manifest.validate().is_err());
        manifest.permissions.push(Permission::Process);
        manifest
            .commands
            .push(manifest.commands.first().context("missing")?.clone());
        assert!(manifest.validate().is_err());
        Ok(())
    }
    #[test]
    fn rejects_bad_panel_reference() -> Result<()> {
        let mut manifest = manifest();
        manifest
            .panels
            .first_mut()
            .context("missing")?
            .commands
            .push("missing".into());
        assert!(manifest.validate().is_err());
        Ok(())
    }
    #[test]
    fn template_paths_cannot_escape() -> Result<()> {
        for path in [
            "../secret",
            "/tmp/secret",
            "a/../../secret",
            "a\\secret",
            ".git/config",
            "a//b",
        ] {
            let mut manifest = manifest();
            manifest
                .templates
                .first_mut()
                .context("missing")?
                .files
                .insert(path.into(), "bad".into());
            assert!(manifest.validate().is_err(), "{path}");
        }
        Ok(())
    }
    #[test]
    fn settings_are_literal_arguments() -> Result<()> {
        let command = Command {
            id: "run".into(),
            label: "Run".into(),
            executable: "love".into(),
            args: vec!["${settings.path}".into()],
        };
        let dangerous = "a space; $(touch /tmp/nope)";
        assert_eq!(
            command
                .resolve(&BTreeMap::from([("path".into(), dangerous.into())]))?
                .1,
            vec![dangerous]
        );
        assert!(command.resolve(&BTreeMap::new()).is_err());
        Ok(())
    }
    #[test]
    fn install_update_disable_remove_persist() -> Result<()> {
        let directory = tempfile::tempdir()?;
        install(directory.path(), manifest())?;
        let mut entries = load(directory.path())?;
        let entry = entries.first_mut().context("missing")?;
        assert!(!entry.enabled);
        entry.enabled = true;
        entry.settings.insert(
            "runtime".into(),
            "/Applications/love.app/Contents/MacOS/love".into(),
        );
        save(directory.path(), entry)?;
        assert!(load(directory.path())?.first().context("missing")?.enabled);
        install(directory.path(), manifest())?;
        let updated = load(directory.path())?;
        let entry = updated.first().context("missing")?;
        assert!(!entry.enabled);
        assert!(entry.settings["runtime"].starts_with("/Applications"));
        remove(directory.path(), "love")?;
        assert!(load(directory.path())?.is_empty());
        Ok(())
    }
    #[test]
    fn template_creation_never_overwrites() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let manifest = manifest();
        let template = manifest.templates.first().context("missing")?;
        let path = create_project(parent.path(), template)?;
        fs::write(path.join("main.lua"), "user content")?;
        assert!(create_project(parent.path(), template).is_err());
        assert_eq!(fs::read_to_string(path.join("main.lua"))?, "user content");
        Ok(())
    }
    #[test]
    fn stored_manifest_is_snapshot() -> Result<()> {
        let source = tempfile::tempdir()?;
        let store = tempfile::tempdir()?;
        fs::write(
            source.path().join("hulder-addon.json"),
            serde_json::to_vec(&manifest())?,
        )?;
        install(store.path(), read_manifest(source.path())?)?;
        fs::write(source.path().join("hulder-addon.json"), "invalid")?;
        assert_eq!(load(store.path())?.len(), 1);
        Ok(())
    }
}
