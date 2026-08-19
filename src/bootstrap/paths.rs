use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct BootstrapPaths {
    pub state: PathBuf,
    pub cache: PathBuf,
    pub state_lock: PathBuf,
    pub cache_lock: PathBuf,
    pub cache_wal: PathBuf,
    pub cache_shm: PathBuf,
    pub cache_journal: PathBuf,
}

impl BootstrapPaths {
    pub fn resolve(state: &Path, cache: Option<&Path>) -> Result<Self> {
        let state = normalize(state).context("normalizing --state")?;
        let cache = match cache {
            Some(path) => normalize(path).context("normalizing --cache")?,
            None => normalize(&crate::map::appended_path(&state, ".bootstrap.sqlite"))
                .context("normalizing default bootstrap cache")?,
        };
        // These names are part of SQLite's protocol and must stay beside the
        // normalized cache. Canonicalizing an existing final component here
        // would turn a sidecar symlink into authority over its target.
        let paths = Self {
            state_lock: crate::map::lock_path(&state),
            cache_lock: crate::map::appended_path(&cache, ".lock"),
            cache_wal: crate::map::appended_path(&cache, "-wal"),
            cache_shm: crate::map::appended_path(&cache, "-shm"),
            cache_journal: crate::map::appended_path(&cache, "-journal"),
            state,
            cache,
        };
        paths.reject_unsafe_derived_artifacts()?;
        paths.reject_aliases()?;
        Ok(paths)
    }

    fn reject_unsafe_derived_artifacts(&self) -> Result<()> {
        for (name, path) in [
            ("state lock", &self.state_lock),
            ("cache lock", &self.cache_lock),
            ("cache WAL", &self.cache_wal),
            ("cache SHM", &self.cache_shm),
            ("cache rollback journal", &self.cache_journal),
        ] {
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("inspecting {name} {path:?}"));
                }
            };
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "{name} {path:?} is a symlink; refusing an unsafe derived artifact"
            );
            anyhow::ensure!(
                metadata.is_file(),
                "{name} {path:?} is not a regular file; refusing an unsafe derived artifact"
            );
        }
        Ok(())
    }

    fn reject_aliases(&self) -> Result<()> {
        let named = [
            ("state", &self.state),
            ("state lock", &self.state_lock),
            ("cache", &self.cache),
            ("cache lock", &self.cache_lock),
            ("cache WAL", &self.cache_wal),
            ("cache SHM", &self.cache_shm),
            ("cache rollback journal", &self.cache_journal),
        ];
        for left in 0..named.len() {
            for right in left + 1..named.len() {
                let (left_name, left_path) = named[left];
                let (right_name, right_path) = named[right];
                anyhow::ensure!(
                    !same_artifact(left_path, right_path)?,
                    "bootstrap artifact collision: {left_name} {left_path:?} aliases {right_name} {right_path:?}"
                );
            }
        }
        Ok(())
    }
}

/// Canonicalize the nearest existing ancestor, then append the not-yet-created
/// suffix lexically. This resolves symlinked directories without requiring the
/// destination itself to exist.
pub fn normalize(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(!path.as_os_str().is_empty(), "path is empty");
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::<PathBuf>::new();
    loop {
        if std::fs::symlink_metadata(ancestor).is_ok() {
            break;
        }
        let parent = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("no existing ancestor for {absolute:?}"))?;
        let component = ancestor
            .strip_prefix(parent)
            .with_context(|| format!("splitting nonexistent suffix from {ancestor:?}"))?;
        suffix.push(component.to_path_buf());
        ancestor = parent;
    }

    let mut normalized = std::fs::canonicalize(ancestor)
        .with_context(|| format!("canonicalizing existing ancestor {ancestor:?}"))?;
    for component in suffix.iter().rev() {
        append_lexically(&mut normalized, component)?;
    }
    Ok(normalized)
}

fn append_lexically(base: &mut PathBuf, suffix: &Path) -> Result<()> {
    for component in suffix.components() {
        match component {
            Component::Normal(part) => base.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::ensure!(base.pop(), "path suffix escapes filesystem root");
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("not-yet-existing path suffix unexpectedly became absolute")
            }
        }
    }
    Ok(())
}

fn same_artifact(left: &Path, right: &Path) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    let (Ok(left_meta), Ok(right_meta)) = (std::fs::metadata(left), std::fs::metadata(right))
    else {
        return Ok(false);
    };
    same_file_identity(&left_meta, &right_meta)
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "usdt-pir-bootstrap-path-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn default_cache_appends_to_the_complete_state_name() {
        let root = root("default");
        let paths = BootstrapPaths::resolve(&root.join("balances.snapshot"), None).unwrap();
        assert_eq!(
            paths.cache.file_name().unwrap(),
            "balances.snapshot.bootstrap.sqlite"
        );
        assert_eq!(
            paths.cache_lock.file_name().unwrap(),
            "balances.snapshot.bootstrap.sqlite.lock"
        );
    }

    #[test]
    fn direct_artifact_collisions_are_rejected() {
        let root = root("collision");
        let state = root.join("balances.snapshot");
        let error = BootstrapPaths::resolve(&state, Some(&state)).unwrap_err();
        assert!(format!("{error:#}").contains("collision"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_existing_ancestors_resolve_to_one_path() {
        let root = root("symlink");
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.join("alias")).unwrap();
        assert_eq!(
            normalize(&root.join("alias/new.snapshot")).unwrap(),
            normalize(&real.join("new.snapshot")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_hard_links_are_rejected_by_filesystem_identity() {
        let root = root("hard-link");
        let state = root.join("balances.snapshot");
        let cache = root.join("cache.sqlite");
        std::fs::write(&state, b"same inode").unwrap();
        std::fs::hard_link(&state, &cache).unwrap();
        let error = BootstrapPaths::resolve(&state, Some(&cache)).unwrap_err();
        assert!(format!("{error:#}").contains("aliases"));
    }

    #[cfg(unix)]
    #[test]
    fn derived_sidecar_symlinks_are_rejected_without_resolving_the_target() {
        let root = root("sidecar-symlink");
        let state = root.join("balances.snapshot");
        let cache = root.join("cache.sqlite");
        let target = root.join("operator-data");
        std::fs::write(&target, b"operator bytes").unwrap();
        std::os::unix::fs::symlink(&target, root.join("cache.sqlite-wal")).unwrap();

        let error = BootstrapPaths::resolve(&state, Some(&cache)).unwrap_err();
        assert!(format!("{error:#}").contains("cache WAL"));
        assert!(format!("{error:#}").contains("symlink"));
        assert_eq!(std::fs::read(&target).unwrap(), b"operator bytes");
        assert!(
            std::fs::symlink_metadata(root.join("cache.sqlite-wal"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn cache_lock_is_appended_and_exclusive() {
        let root = root("cache-lock");
        let paths = BootstrapPaths::resolve(&root.join("balances.snapshot"), None).unwrap();
        assert_eq!(
            paths.cache_lock.file_name().unwrap(),
            "balances.snapshot.bootstrap.sqlite.lock"
        );
        let _held = crate::map::AdvisoryLock::acquire(&paths.cache_lock, "testing cache").unwrap();
        let error =
            crate::map::AdvisoryLock::acquire(&paths.cache_lock, "testing cache").unwrap_err();
        assert!(format!("{error:#}").contains("another process"));
    }
}
