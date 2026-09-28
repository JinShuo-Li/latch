//! Shared path resolution for workspace tools and shell inspection policy.

use anyhow::{Result, anyhow, bail};
use std::path::{Path, PathBuf};

/// Resolve aliases on the deepest existing ancestor without accepting a
/// dangling symlink as a missing leaf.
pub(crate) fn resolve_real_path(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path.to_path_buf();
    let mut tail = Vec::new();
    loop {
        if let Ok(mut real) = ancestor.canonicalize() {
            for component in tail.iter().rev() {
                real.push(component);
            }
            return Some(real);
        }
        if std::fs::symlink_metadata(&ancestor).is_ok() {
            return None;
        }
        tail.push(ancestor.file_name()?.to_os_string());
        if !ancestor.pop() {
            return None;
        }
    }
}

pub(crate) fn resolve_workspace_path(workspace: &Path, path: &str) -> Result<PathBuf> {
    let root = workspace.canonicalize()?;
    let candidate = if Path::new(path).is_absolute() {
        lexical_normalize(Path::new(path))
    } else {
        lexical_normalize(&root.join(path))
    };
    let resolved = resolve_real_path(&candidate).ok_or_else(|| anyhow!("invalid path"))?;
    if !resolved.starts_with(&root) {
        bail!("path escapes workspace through a symbolic link or parent traversal");
    }
    Ok(resolved)
}

pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// NTFS hardlinks have no canonical origin. A kernel-native file read must
/// refuse an alias whose other name could refer to a protected secret.
pub(crate) fn file_alias_may_escape(path: &Path) -> bool {
    #[cfg(windows)]
    {
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && windows_has_multiple_links(path))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

#[cfg(windows)]
fn windows_has_multiple_links(path: &Path) -> bool {
    use winsafe::{HFILE, co};

    let Some(path) = path.to_str() else {
        return true;
    };
    let Ok((file, _)) = HFILE::CreateFile(
        path,
        co::GENERIC::READ,
        Some(co::FILE_SHARE::READ | co::FILE_SHARE::WRITE | co::FILE_SHARE::DELETE),
        None,
        co::DISPOSITION::OPEN_EXISTING,
        co::FILE_ATTRIBUTE::NORMAL,
        None,
        None,
        None,
    ) else {
        return true;
    };
    file.GetFileInformationByHandle()
        .map_or(true, |information| information.nNumberOfLinks > 1)
}
