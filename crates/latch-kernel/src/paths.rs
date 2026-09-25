//! The single place that selects Latch's configuration and storage paths.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    Explicit,
    New,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyPaths {
    pub config_path: PathBuf,
    pub state_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub config_path: PathBuf,
    pub state_root: PathBuf,
    pub secrets_path: PathBuf,
    pub database_path: PathBuf,
    pub artifacts_root: PathBuf,
    pub cache_root: PathBuf,
    pub source: PathSource,
    pub legacy: Option<LegacyPaths>,
}

impl ResolvedPaths {
    #[must_use]
    pub fn resolve(explicit_config: Option<&Path>, state_override: Option<&Path>) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let legacy_config_root = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let legacy_state_root = dirs::state_dir()
            .unwrap_or_else(|| home.join(".local/state"))
            .join("latch");
        Self::resolve_with_roots(
            &home,
            &legacy_config_root,
            &legacy_state_root,
            explicit_config,
            state_override,
        )
    }

    #[must_use]
    pub fn resolve_with_roots(
        home: &Path,
        legacy_config_root: &Path,
        legacy_state_root: &Path,
        explicit_config: Option<&Path>,
        state_override: Option<&Path>,
    ) -> Self {
        let root = home.join(".latch");
        let new_config = root.join("config.toml");
        let legacy_config = legacy_config_root.join("latch/config.toml");
        let source = if explicit_config.is_some() {
            PathSource::Explicit
        } else if !new_config.exists() && legacy_config.exists() {
            PathSource::Legacy
        } else {
            PathSource::New
        };
        let config_path = explicit_config.map_or_else(
            || match source {
                PathSource::Legacy => legacy_config.clone(),
                PathSource::Explicit | PathSource::New => new_config,
            },
            Path::to_path_buf,
        );
        let state_root = state_override.map_or_else(
            || match source {
                PathSource::Legacy => legacy_state_root.to_path_buf(),
                PathSource::Explicit | PathSource::New => root,
            },
            Path::to_path_buf,
        );
        let legacy = (source == PathSource::Legacy).then(|| LegacyPaths {
            config_path: legacy_config,
            state_root: legacy_state_root.to_path_buf(),
        });
        Self::from_parts(config_path, state_root, source, legacy)
    }

    /// Derive storage children for a resolved state root without repeating
    /// filename construction at call sites.
    #[must_use]
    pub fn for_state(state_root: &Path) -> Self {
        Self::from_parts(
            state_root.join("config.toml"),
            state_root.to_path_buf(),
            PathSource::Explicit,
            None,
        )
    }

    fn from_parts(
        config_path: PathBuf,
        state_root: PathBuf,
        source: PathSource,
        legacy: Option<LegacyPaths>,
    ) -> Self {
        Self {
            config_path,
            secrets_path: state_root.join("secrets.toml"),
            database_path: state_root.join("latch.sqlite3"),
            artifacts_root: state_root.join("artifacts"),
            cache_root: state_root.join("cache"),
            state_root,
            source,
            legacy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_layout_and_explicit_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let legacy_config = home.join(".config/latch");
        std::fs::create_dir_all(&legacy_config).unwrap();
        std::fs::write(legacy_config.join("config.toml"), "").unwrap();
        let legacy_state = home.join(".local/state/latch");
        let legacy = ResolvedPaths::resolve_with_roots(
            home,
            &home.join(".config"),
            &legacy_state,
            None,
            None,
        );
        assert_eq!(legacy.source, PathSource::Legacy);
        assert_eq!(legacy.state_root, legacy_state);
        assert_eq!(legacy.secrets_path, legacy_state.join("secrets.toml"));

        let explicit = home.join("my-config.toml");
        let selected = ResolvedPaths::resolve_with_roots(
            home,
            &home.join(".config"),
            &legacy_state,
            Some(&explicit),
            Some(&home.join("my-state")),
        );
        assert_eq!(selected.source, PathSource::Explicit);
        assert_eq!(selected.config_path, explicit);
        assert_eq!(selected.database_path, home.join("my-state/latch.sqlite3"));

        std::fs::create_dir_all(home.join(".latch")).unwrap();
        std::fs::write(home.join(".latch/config.toml"), "").unwrap();
        let new = ResolvedPaths::resolve_with_roots(
            home,
            &home.join(".config"),
            &legacy_state,
            None,
            None,
        );
        assert_eq!(new.source, PathSource::New);
        assert_eq!(new.config_path, home.join(".latch/config.toml"));
        assert_eq!(new.artifacts_root, home.join(".latch/artifacts"));
        assert_eq!(new.cache_root, home.join(".latch/cache"));
    }
}
