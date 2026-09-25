//! Explicit, recoverable migration from the XDG layout to `~/.latch`.

use crate::config::Config;
use crate::credentials::CredentialStore;
use crate::paths::{PathSource, ResolvedPaths};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, MAIN_DB, OpenFlags};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Serialize)]
struct MigrationMarker<'a> {
    source_config: &'a str,
    source_state: &'a str,
    migrated_at: String,
}

/// Copy and publish a legacy installation. The old live filenames are renamed
/// only after the new config and a non-resurrection marker are durable.
pub fn migrate_legacy(source: &ResolvedPaths, target: &ResolvedPaths) -> Result<()> {
    if source.source != PathSource::Legacy {
        bail!("no live legacy XDG configuration to migrate");
    }
    if target.config_path.exists() {
        bail!(
            "target configuration already exists at {}",
            target.config_path.display()
        );
    }
    let legacy = source.legacy.as_ref().expect("legacy source has paths");
    target
        .ensure_state_root()
        .context("create private Latch root")?;

    // Prepare all recoverable data before publishing the config. A failed copy
    // leaves the old installation live and may leave harmless target files.
    let source_text = std::fs::read_to_string(&source.config_path)?;
    let has_state_override = source_text
        .parse::<toml::Table>()?
        .contains_key("state_dir");
    let mut config = Config::load(Some(&source.config_path))?;
    if !has_state_override {
        config.state_dir = target.state_root.clone();
    }
    let secrets = if source.secrets_path.exists() {
        CredentialStore::open(&source.secrets_path)?;
        Some(std::fs::read(&source.secrets_path).context("read legacy secrets")?)
    } else {
        None
    };
    if source.database_path.exists() {
        let db =
            Connection::open_with_flags(&source.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("open legacy session database")?;
        let staged_db = tempfile::NamedTempFile::new_in(&target.state_root)?;
        db.backup(MAIN_DB, staged_db.path(), None)
            .context("back up legacy session database")?;
        staged_db
            .persist(&target.database_path)
            .map_err(|e| e.error)?;
    }
    if source.artifacts_root.exists() {
        copy_tree(&source.artifacts_root, &target.artifacts_root)?;
    }
    if let Some(bytes) = secrets {
        stage_bytes(&target.secrets_path, &bytes)?;
    }

    let staged_config = target.state_root.join("config.migrating.toml");
    config.save(&staged_config)?;
    std::fs::rename(&staged_config, &target.config_path).context("publish migrated config")?;
    sync_dir(&target.state_root)?;

    let marker = MigrationMarker {
        source_config: legacy
            .config_path
            .to_str()
            .context("legacy config path is not UTF-8")?,
        source_state: legacy
            .state_root
            .to_str()
            .context("legacy state path is not UTF-8")?,
        migrated_at: chrono::Utc::now().to_rfc3339(),
    };
    stage_bytes(
        &target.migration_marker(),
        toml::to_string(&marker)?.as_bytes(),
    )?;

    // A marker makes the old exact filenames non-discoverable even if a backup
    // rename is interrupted. Keep the old files recoverable under explicit names.
    rename_backup(&source.config_path)?;
    if source.secrets_path.exists() {
        rename_backup(&source.secrets_path)?;
    }
    Ok(())
}

fn rename_backup(path: &Path) -> Result<()> {
    let backup = path.with_extension("toml.migrated-backup");
    if backup.exists() {
        bail!("migration backup already exists at {}", backup.display());
    }
    std::fs::rename(path, &backup)
        .with_context(|| format!("preserve legacy backup at {}", backup.display()))?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn stage_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("target has no parent directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    sync_dir(parent)
}

fn sync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    ResolvedPaths::ensure_private_root(target)?;
    for item in std::fs::read_dir(source)? {
        let item = item?;
        let source_path = item.path();
        let target_path = target.join(item.file_name());
        let kind = item.file_type()?;
        if kind.is_dir() {
            copy_tree(&source_path, &target_path)?;
        } else if kind.is_file() {
            std::fs::copy(&source_path, &target_path)?;
        } else {
            bail!(
                "legacy artifact {} is not a regular file",
                source_path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_database_artifacts_and_secrets_without_legacy_resurrection() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let legacy_config_root = home.join(".config");
        let legacy_config = legacy_config_root.join("latch/config.toml");
        let legacy_state = home.join(".local/state/latch");
        std::fs::create_dir_all(legacy_config.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy_state.join("artifacts/session")).unwrap();
        std::fs::write(
            &legacy_config,
            "[provider]\nkind = 'deepseek'\nmodel = 'deepseek-flash'\n",
        )
        .unwrap();
        let mut secrets = CredentialStore::open(legacy_state.join("secrets.toml")).unwrap();
        secrets.set("deepseek", "private").unwrap();
        std::fs::write(legacy_state.join("artifacts/session/data"), "artifact").unwrap();
        let db = Connection::open(legacy_state.join("latch.sqlite3")).unwrap();
        db.execute("CREATE TABLE evidence (value TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO evidence VALUES ('kept')", [])
            .unwrap();
        drop(db);

        let source =
            ResolvedPaths::resolve_with_roots(home, &legacy_config_root, &legacy_state, None, None);
        let target = ResolvedPaths::for_state(&home.join(".latch"));
        migrate_legacy(&source, &target).unwrap();
        assert!(target.migration_marker().exists());
        assert!(target.config_path.exists());
        assert!(!legacy_config.exists());
        assert!(
            legacy_config
                .with_extension("toml.migrated-backup")
                .exists()
        );
        assert!(!legacy_state.join("secrets.toml").exists());
        assert_eq!(
            std::fs::read_to_string(target.artifacts_root.join("session/data")).unwrap(),
            "artifact"
        );
        let new_db = Connection::open(&target.database_path).unwrap();
        let value: String = new_db
            .query_row("SELECT value FROM evidence", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "kept");
        let restored = Config::load(Some(&target.config_path)).unwrap();
        assert_eq!(restored.state_dir, target.state_root);
        assert_eq!(
            CredentialStore::open(&target.secrets_path)
                .unwrap()
                .require(&"file:deepseek".parse().unwrap())
                .unwrap(),
            "private"
        );

        std::fs::remove_file(&target.config_path).unwrap();
        let resolved =
            ResolvedPaths::resolve_with_roots(home, &legacy_config_root, &legacy_state, None, None);
        assert_eq!(resolved.source, PathSource::New);
        assert!(!resolved.config_path.exists());
    }

    #[test]
    fn failed_migration_leaves_legacy_live() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let legacy_config_root = home.join(".config");
        let legacy_config = legacy_config_root.join("latch/config.toml");
        let legacy_state = home.join(".local/state/latch");
        std::fs::create_dir_all(legacy_config.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&legacy_state).unwrap();
        std::fs::write(&legacy_config, "[provider]\nkind = 'deepseek'\n").unwrap();
        #[cfg(unix)]
        {
            std::fs::write(home.join("outside"), "do not copy").unwrap();
            std::os::unix::fs::symlink(home.join("outside"), legacy_state.join("artifacts"))
                .unwrap();
        }
        let source =
            ResolvedPaths::resolve_with_roots(home, &legacy_config_root, &legacy_state, None, None);
        let target = ResolvedPaths::for_state(&home.join(".latch"));
        assert!(migrate_legacy(&source, &target).is_err());
        assert!(legacy_config.exists());
        assert!(!target.config_path.exists());
        assert!(!target.migration_marker().exists());
    }
}
