use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Library {
    pub groups: Vec<Group>,
    pub bookmarks: Vec<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub files: Vec<PathBuf>,
}
impl Library {
    pub fn create(&mut self, name: &str, files: Vec<PathBuf>) -> anyhow::Result<()> {
        let name = self.validate_name(name, None)?;
        let mut unique = Vec::new();
        append_unique(&mut unique, files);
        anyhow::ensure!(
            !unique.is_empty(),
            "Open at least one saved local file first."
        );
        self.groups.push(Group {
            name,
            files: unique,
        });
        Ok(())
    }
    pub fn rename(&mut self, index: usize, name: &str) -> anyhow::Result<()> {
        let name = self.validate_name(name, Some(index))?;
        let group = self
            .groups
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("Group no longer exists"))?;
        group.name = name;
        Ok(())
    }
    fn validate_name(&self, name: &str, except: Option<usize>) -> anyhow::Result<String> {
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.chars().count() <= 80,
            "Enter a group name between 1 and 80 characters."
        );
        anyhow::ensure!(
            !self
                .groups
                .iter()
                .enumerate()
                .any(|(i, g)| Some(i) != except && g.name.to_lowercase() == name.to_lowercase()),
            "A group with this name already exists."
        );
        Ok(name.to_owned())
    }
}
pub fn append_unique(target: &mut Vec<PathBuf>, files: Vec<PathBuf>) {
    for file in files {
        if file.is_absolute() && !target.contains(&file) {
            target.push(file);
        }
    }
}
pub fn load(path: &Path) -> anyhow::Result<Library> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Library::default()),
        Err(error) => Err(error.into()),
    }
}
pub fn save(path: &Path, library: &Library) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid library path"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&serde_json::to_vec_pretty(library)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_and_order() {
        let mut library = Library::default();
        library
            .create(" Work ", vec!["/b".into(), "/a".into(), "/b".into()])
            .unwrap();
        assert_eq!(
            library.groups[0].files,
            vec![PathBuf::from("/b"), PathBuf::from("/a")]
        );
        assert!(library.create("work", vec!["/c".into()]).is_err());
        assert!(library.create("", vec!["/c".into()]).is_err());
        assert!(library.create("Empty", vec!["relative".into()]).is_err());
        library.rename(0, "Renamed").unwrap();
        assert_eq!(library.groups[0].name, "Renamed");
    }
    #[test]
    fn invalid_rename_preserves_both_groups() {
        let mut library = Library::default();
        library.create("First", vec!["/one".into()]).unwrap();
        library.create("Second", vec!["/two".into()]).unwrap();
        assert!(library.rename(0, "second").is_err());
        assert!(library.rename(0, &"x".repeat(81)).is_err());
        assert!(library.rename(7, "No group").is_err());
        assert_eq!(library.groups[0].name, "First");
        assert_eq!(library.groups[1].name, "Second");
    }
    #[test]
    fn seven_files_survive_reordering_and_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("groups.json");
        let files: Vec<PathBuf> = (0..7)
            .map(|index| directory.path().join(format!("file{index}.rs")))
            .collect();
        for file in &files {
            std::fs::write(file, "original").unwrap();
        }
        let mut library = Library::default();
        library.create("Seven tabs", files.clone()).unwrap();
        library.groups[0].files.swap(0, 6);
        append_unique(&mut library.groups[0].files, files.clone());
        save(&path, &library).unwrap();
        let mut restored = load(&path).unwrap();
        assert_eq!(restored.groups[0].files.len(), 7);
        assert_eq!(restored.groups[0].files[0], files[6]);
        restored.groups.remove(0);
        save(&path, &restored).unwrap();
        assert!(load(&path).unwrap().groups.is_empty());
        assert!(
            files
                .iter()
                .all(|file| std::fs::read_to_string(file).unwrap() == "original")
        );
    }
    #[test]
    fn roundtrip_and_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("groups.json");
        let mut library = load(&path).unwrap();
        library.create("One", vec!["/missing/file".into()]).unwrap();
        append_unique(&mut library.bookmarks, vec!["/a".into(), "/a".into()]);
        save(&path, &library).unwrap();
        assert_eq!(
            load(&path).unwrap().groups[0].files[0],
            PathBuf::from("/missing/file")
        );
        assert_eq!(load(&path).unwrap().bookmarks.len(), 1);
        std::fs::write(&path, b"broken").unwrap();
        assert!(load(&path).is_err());
    }
}
