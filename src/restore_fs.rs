use anyhow::{Context, Result};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, MetadataExt, OpenOptions, OpenOptionsExt, Permissions, PermissionsExt};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::models::WatchedProject;

/// A restore target whose authority is confined to the watched project.
///
/// The path is deliberately relative and contains only normal components. It
/// may traverse parent symlinks, but cap-std guarantees those links cannot
/// escape the project capability.
#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ProjectPath {
    relative: PathBuf,
}

impl ProjectPath {
    pub(crate) fn from_absolute(project_root: &Path, path: &Path) -> Result<Self> {
        let relative = path.strip_prefix(project_root).with_context(|| {
            format!(
                "restore target '{}' is outside the project root '{}'",
                path.display(),
                project_root.display()
            )
        })?;
        Self::from_relative(relative)
    }

    pub(crate) fn from_stored(project: &WatchedProject, path: &str) -> Result<Self> {
        Self::from_absolute(Path::new(&project.root_path), Path::new(path))
    }

    pub(crate) fn from_relative(path: &Path) -> Result<Self> {
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => relative.push(name),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    anyhow::bail!(
                        "restore target '{}' is not a confined project-relative path",
                        path.display()
                    )
                }
            }
        }
        if relative.as_os_str().is_empty() {
            anyhow::bail!("restore target must name a file inside the project");
        }
        Ok(Self { relative })
    }

    pub(crate) fn relative(&self) -> &Path {
        &self.relative
    }

    pub(crate) fn display(&self) -> String {
        self.relative.to_string_lossy().to_string()
    }

    pub(crate) fn absolute(&self, project: &WatchedProject) -> PathBuf {
        Path::new(&project.root_path).join(&self.relative)
    }

    fn parent(&self) -> &Path {
        self.relative.parent().unwrap_or_else(|| Path::new(""))
    }

    fn file_name(&self) -> &OsStr {
        self.relative
            .file_name()
            .expect("validated project paths always have a file name")
    }
}

/// Capability-scoped access to a watched project and Undo's private data store.
pub(crate) struct RestoreFs {
    project: Dir,
    project_root: PathBuf,
}

pub(crate) enum CappedRead {
    Missing,
    TooLarge,
    Content(Vec<u8>),
}

impl RestoreFs {
    pub(crate) fn open(project: &WatchedProject) -> Result<Self> {
        let project_root = PathBuf::from(&project.root_path);
        // The capability bootstrap is the only ambient project lookup. Walk
        // every component without following symlinks, then keep all descendant
        // access behind cap-std's confined resolver.
        let project_dir = open_absolute_dir_nofollow(&project_root)
            .with_context(|| format!("open project root '{}'", project_root.display()))?;
        Ok(Self {
            project: project_dir,
            project_root,
        })
    }

    /// Validate the current namespace without relying on it remaining unchanged.
    /// Mutation safety comes from retaining capability handles during each write.
    pub(crate) fn validate(&self, target: &ProjectPath) -> Result<()> {
        let Some((parent, name)) = self.open_parent(target, false)? else {
            return Ok(());
        };
        match parent.symlink_metadata(&name) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing to restore through symlink '{}'",
                    target.relative().display()
                )
            }
            Ok(metadata) if metadata.is_dir() => {
                anyhow::bail!(
                    "restore target '{}' is a directory",
                    target.relative().display()
                )
            }
            Ok(metadata) if !metadata.is_file() => {
                anyhow::bail!(
                    "restore target '{}' is not a regular file",
                    target.relative().display()
                )
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!("validate restore target '{}'", target.relative().display())
            }),
        }
    }

    pub(crate) fn read_capped(&self, target: &ProjectPath, limit: usize) -> Result<CappedRead> {
        let Some((parent, name)) = self.open_parent(target, false)? else {
            return Ok(CappedRead::Missing);
        };
        let Some(mut file) = open_existing_file(&parent, &name, target)? else {
            return Ok(CappedRead::Missing);
        };
        if file.metadata()?.len() > limit as u64 {
            return Ok(CappedRead::TooLarge);
        }
        let mut content = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(limit.saturating_add(1) as u64)
            .read_to_end(&mut content)?;
        if content.len() > limit {
            return Ok(CappedRead::TooLarge);
        }
        Ok(CappedRead::Content(content))
    }

    pub(crate) fn exists(&self, target: &ProjectPath) -> Result<bool> {
        let Some((parent, name)) = self.open_parent(target, false)? else {
            return Ok(false);
        };
        match parent.symlink_metadata(&name) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing to restore through symlink '{}'",
                    target.relative().display()
                )
            }
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| {
                format!("inspect restore target '{}'", target.relative().display())
            }),
        }
    }

    /// Atomically replace one project file and return the safety backup path,
    /// when an existing file was replaced.
    pub(crate) fn write(&self, target: &ProjectPath, content: &[u8]) -> Result<Option<PathBuf>> {
        let (parent, name) = self
            .open_parent(target, true)?
            .expect("creating the parent always yields a directory capability");
        let existing = open_existing_file(&parent, &name, target)?;
        let (backup_path, mode, identity) = if let Some(mut current) = existing {
            let metadata = current.metadata()?;
            let mode = metadata.mode();
            let identity = Some((metadata.dev(), metadata.ino()));
            let backup = self.back_up(target, &mut current)?;
            (Some(backup), mode, identity)
        } else {
            (None, 0o600, None)
        };

        let (temp_name, mut temp) = create_temp_file(&parent, &name)?;
        let write_result = (|| -> Result<()> {
            temp.write_all(content)?;
            temp.set_permissions(Permissions::from_mode(mode))?;
            temp.sync_all()?;
            ensure_leaf_unchanged(&parent, &name, target, identity)?;
            parent.rename(&temp_name, &parent, &name)?;
            // The mutation has already succeeded if this best-effort durability
            // sync is unsupported by the filesystem.
            let _ = sync_dir(&parent);
            Ok(())
        })();
        if write_result.is_err() {
            let _ = parent.remove_file(&temp_name);
        }
        write_result?;
        Ok(backup_path)
    }

    /// Delete one project file after preserving its exact opened contents.
    pub(crate) fn delete(&self, target: &ProjectPath) -> Result<Option<PathBuf>> {
        let Some((parent, name)) = self.open_parent(target, false)? else {
            return Ok(None);
        };
        let Some(mut current) = open_existing_file(&parent, &name, target)? else {
            return Ok(None);
        };
        let opened = current.metadata()?;
        let backup = self.back_up(target, &mut current)?;

        // Detect the common leaf-swap race. A change after this check cannot
        // escape the retained parent capability; remove_file never follows the
        // final component.
        ensure_leaf_unchanged(&parent, &name, target, Some((opened.dev(), opened.ino())))?;
        parent.remove_file(&name)?;
        let _ = sync_dir(&parent);
        Ok(Some(backup))
    }

    fn open_parent(&self, target: &ProjectPath, create: bool) -> Result<Option<(Dir, OsString)>> {
        let parent_path = target.parent();
        let parent = if parent_path.as_os_str().is_empty() {
            self.project.try_clone()?
        } else {
            if create {
                self.project.create_dir_all(parent_path).with_context(|| {
                    format!(
                        "create parent for restore target '{}'",
                        target.relative().display()
                    )
                })?;
            }
            match self.project.open_dir(parent_path) {
                Ok(parent) => parent,
                Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "open parent for restore target '{}'",
                            target.relative().display()
                        )
                    });
                }
            }
        };
        Ok(Some((parent, target.file_name().to_os_string())))
    }

    fn back_up(&self, target: &ProjectPath, current: &mut cap_std::fs::File) -> Result<PathBuf> {
        let data_path = crate::backtrack_dir_path()?;
        if data_path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            anyhow::bail!("Undo data directory '{}' is a symlink", data_path.display());
        }
        let data_root = crate::backtrack_dir()?;
        let canonical_data_root = data_root.canonicalize()?;
        let data = open_absolute_dir_nofollow(&canonical_data_root).with_context(|| {
            format!(
                "open Undo data directory '{}'",
                canonical_data_root.display()
            )
        })?;
        data.create_dir_all("backups")?;
        data.set_permissions("backups", Permissions::from_mode(0o700))?;
        let backups = data.open_dir("backups")?;
        let timestamp = timestamp_nanos();
        for attempt in 0..1000 {
            let name = backup_name(&self.project_root, target, timestamp, attempt);
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .mode(0o600)
                .follow(FollowSymlinks::No);
            let mut backup = match backups.open_with(&name, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            std::io::copy(current, &mut backup)?;
            backup.set_permissions(Permissions::from_mode(0o600))?;
            backup.sync_all()?;
            sync_dir(&backups)?;
            return Ok(canonical_data_root.join("backups").join(name));
        }
        anyhow::bail!(
            "could not create a unique restore backup for {}",
            target.relative().display()
        )
    }
}

fn open_absolute_dir_nofollow(path: &Path) -> Result<Dir> {
    if !path.is_absolute() {
        anyhow::bail!(
            "directory capability path '{}' is not absolute",
            path.display()
        );
    }
    let mut dir = Dir::open_ambient_dir(Path::new("/"), ambient_authority())?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "directory capability path '{}' contains an unsafe component",
                    path.display()
                )
            }
        };
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .follow(FollowSymlinks::No);
        let file = dir.open_with(name, &options)?;
        dir = Dir::from_std_file(file.into_std());
    }
    Ok(dir)
}

fn open_existing_file(
    parent: &Dir,
    name: &OsStr,
    target: &ProjectPath,
) -> Result<Option<cap_std::fs::File>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .follow(FollowSymlinks::No);
    match parent.open_with(name, &options) {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                anyhow::bail!(
                    "restore target '{}' is not a regular file",
                    target.relative().display()
                );
            }
            Ok(Some(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            if parent
                .symlink_metadata(name)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                anyhow::bail!(
                    "refusing to restore through symlink '{}'",
                    target.relative().display()
                );
            }
            Err(error)
                .with_context(|| format!("open restore target '{}'", target.relative().display()))
        }
    }
}

fn ensure_leaf_unchanged(
    parent: &Dir,
    name: &OsStr,
    target: &ProjectPath,
    expected: Option<(u64, u64)>,
) -> Result<()> {
    match (expected, parent.symlink_metadata(name)) {
        (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        (Some((device, inode)), Ok(metadata))
            if metadata.is_file() && metadata.dev() == device && metadata.ino() == inode =>
        {
            Ok(())
        }
        (_, Err(error)) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => anyhow::bail!(
            "restore target '{}' changed before this file could be updated",
            target.relative().display()
        ),
    }
}

fn create_temp_file(parent: &Dir, target_name: &OsStr) -> Result<(OsString, cap_std::fs::File)> {
    let timestamp = timestamp_nanos();
    for attempt in 0..1000 {
        let mut name = target_name.to_os_string();
        name.push(format!(
            ".undo.partial.{}_{}.{}",
            std::process::id(),
            timestamp,
            attempt
        ));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .follow(FollowSymlinks::No);
        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("could not create a unique restore temporary file")
}

fn timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn backup_name(
    project_root: &Path,
    target: &ProjectPath,
    timestamp: u128,
    attempt: usize,
) -> OsString {
    use std::os::unix::ffi::OsStrExt;

    let absolute = project_root.join(target.relative());
    let digest = Sha256::digest(absolute.as_os_str().as_bytes());
    let hash = crate::to_hex(&digest[..8]);
    let filename = target.file_name().to_string_lossy().into_owned();
    let retry = if attempt == 0 {
        String::new()
    } else {
        format!(".{attempt}")
    };
    OsString::from(format!("{filename}_{hash}_{timestamp}{retry}.bak"))
}

fn sync_dir(dir: &Dir) -> std::io::Result<()> {
    dir.try_clone()?.into_std_file().sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as StdPermissionsExt, symlink};

    fn project(root: &Path) -> WatchedProject {
        WatchedProject {
            id: 1,
            root_path: root.to_string_lossy().to_string(),
            created_at: 0,
        }
    }

    #[test]
    fn project_path_rejects_escape_components_and_prefix_siblings() {
        let root = Path::new("/project");
        assert!(ProjectPath::from_absolute(root, Path::new("/project/src/main.rs")).is_ok());
        assert!(ProjectPath::from_absolute(root, Path::new("/project-old/file")).is_err());
        assert!(ProjectPath::from_absolute(root, Path::new("/outside/file")).is_err());
        assert!(ProjectPath::from_relative(Path::new("src/../../outside")).is_err());
        assert!(ProjectPath::from_relative(Path::new(".")).is_err());
    }

    #[test]
    fn project_root_symlink_is_rejected_at_capability_bootstrap() {
        let container = tempfile::tempdir().unwrap();
        let container_root = container.path().canonicalize().unwrap();
        let root = container_root.join("project");
        let moved = container.path().join("moved-project");
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(&root).unwrap();
        let project = project(&root);

        std::fs::rename(&root, &moved).unwrap();
        symlink(outside.path(), &root).unwrap();

        assert!(RestoreFs::open(&project).is_err());
    }

    #[test]
    fn project_root_ancestor_symlink_is_rejected_at_capability_bootstrap() {
        let container = tempfile::tempdir().unwrap();
        let container_root = container.path().canonicalize().unwrap();
        let parent = container_root.join("parent");
        let root = parent.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let project = project(&root);
        let moved_parent = container_root.join("moved-parent");
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("project")).unwrap();

        std::fs::rename(&parent, &moved_parent).unwrap();
        symlink(outside.path(), &parent).unwrap();

        assert!(RestoreFs::open(&project).is_err());
    }

    #[test]
    fn outside_parent_symlink_cannot_be_read_written_or_deleted() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel.txt");
        std::fs::write(&sentinel, "outside").unwrap();
        symlink(outside.path(), root.join("linked")).unwrap();

        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_relative(Path::new("linked/sentinel.txt")).unwrap();

        assert!(fs.read_capped(&target, 1024).is_err());
        assert!(fs.write(&target, b"attacker content").is_err());
        assert!(fs.delete(&target).is_err());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "outside");
    }

    #[test]
    fn parent_swap_after_open_cannot_redirect_a_write() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let original_dir = root.join("nested");
        std::fs::create_dir(&original_dir).unwrap();
        std::fs::write(original_dir.join("file.txt"), "inside").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("file.txt");
        std::fs::write(&outside_file, "outside").unwrap();

        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_relative(Path::new("nested/file.txt")).unwrap();

        std::fs::rename(&original_dir, root.join("moved")).unwrap();
        symlink(outside.path(), &original_dir).unwrap();

        assert!(fs.write(&target, b"restored").is_err());
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "outside");
        assert_eq!(
            std::fs::read_to_string(root.join("moved/file.txt")).unwrap(),
            "inside"
        );
    }

    #[test]
    fn confined_parent_symlink_is_allowed_but_leaf_symlink_is_rejected() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        symlink("real", root.join("alias")).unwrap();
        let real_file = root.join("real/file.txt");
        std::fs::write(&real_file, "before").unwrap();

        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let through_parent = ProjectPath::from_relative(Path::new("alias/new-file.txt")).unwrap();
        fs.write(&through_parent, b"inside").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("real/new-file.txt")).unwrap(),
            "inside"
        );

        symlink("file.txt", root.join("real/leaf-link")).unwrap();
        let leaf = ProjectPath::from_relative(Path::new("real/leaf-link")).unwrap();
        assert!(fs.validate(&leaf).is_err());
        assert!(fs.write(&leaf, b"changed").is_err());
        assert_eq!(std::fs::read_to_string(real_file).unwrap(), "before");
    }

    #[test]
    fn backup_uses_opened_file_and_never_overwrites_an_earlier_copy() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("config.json");
        std::fs::write(&file, "first").unwrap();
        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_relative(Path::new("config.json")).unwrap();

        let first = fs.write(&target, b"second").unwrap().unwrap();
        let second = fs.write(&target, b"third").unwrap().unwrap();

        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        assert_eq!(
            std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn symlinked_undo_data_root_blocks_mutation_before_backup() {
        let data_container = tempfile::tempdir().unwrap();
        let real_data = tempfile::tempdir().unwrap();
        let linked_data = data_container.path().join("undo-data");
        symlink(real_data.path(), &linked_data).unwrap();
        crate::set_test_data_dir(linked_data);

        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("config.json");
        std::fs::write(&file, "original").unwrap();
        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_relative(Path::new("config.json")).unwrap();

        assert!(fs.write(&target, b"changed").is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "original");
    }

    #[test]
    fn backup_names_include_the_full_project_path() {
        let root = Path::new("/project");
        let first = ProjectPath::from_relative(Path::new("pkg-a/config.json")).unwrap();
        let second = ProjectPath::from_relative(Path::new("pkg-b/config.json")).unwrap();

        let first_name = backup_name(root, &first, 123, 0);
        let second_name = backup_name(root, &second, 123, 0);

        assert_ne!(first_name, second_name);
        assert!(first_name.to_string_lossy().starts_with("config.json_"));
    }

    #[test]
    fn missing_nested_write_is_complete_and_owner_only() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_relative(Path::new("new/nested/file.txt")).unwrap();

        assert!(fs.write(&target, b"complete contents").unwrap().is_none());

        let file = root.join("new/nested/file.txt");
        assert_eq!(std::fs::read(&file).unwrap(), b"complete contents");
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn temporary_file_creation_never_removes_an_existing_candidate() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();

        let (first_name, _first) = create_temp_file(&fs.project, OsStr::new("file.txt")).unwrap();
        let (second_name, _second) = create_temp_file(&fs.project, OsStr::new("file.txt")).unwrap();

        assert_ne!(first_name, second_name);
        assert!(fs.project.symlink_metadata(&first_name).is_ok());
        assert!(fs.project.symlink_metadata(&second_name).is_ok());
    }

    #[test]
    fn delete_backs_up_before_removing_the_confined_file() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let file = root.join("delete-me.txt");
        std::fs::write(&file, "preserve me").unwrap();
        let project = project(&root);
        let fs = RestoreFs::open(&project).unwrap();
        let target = ProjectPath::from_relative(Path::new("delete-me.txt")).unwrap();

        let backup = fs.delete(&target).unwrap().unwrap();

        assert!(!file.exists());
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "preserve me");
    }
}
