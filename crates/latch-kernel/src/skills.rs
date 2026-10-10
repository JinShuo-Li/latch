//! Agent Skills discovery and bounded, on-demand resource reads.
//! Metadata is session context; loaded text is an ordinary durable tool result.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 1024 * 1024;
const MAX_SKILLS: usize = 512;

#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Skill {
    pub metadata: SkillMetadata,
    pub root: PathBuf,
}
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    pub skills: BTreeMap<String, Skill>,
    pub diagnostics: Vec<String>,
}
impl SkillCatalog {
    /// Workspace Latch, Agents, Claude, then user equivalents; first name wins.
    pub fn discover(workspace: &Path, home: Option<&Path>) -> Self {
        let mut roots = Vec::new();
        for base in [Some(workspace), home].into_iter().flatten() {
            for name in [".latch/skills", ".agents/skills", ".claude/skills"] {
                roots.push(base.join(name));
            }
        }
        Self::from_roots(&roots)
    }
    pub fn from_roots(roots: &[PathBuf]) -> Self {
        let mut catalog = Self::default();
        for root in roots {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    catalog
                        .diagnostics
                        .push(format!("{}: {error}", root.display()));
                    continue;
                }
            };
            let mut paths = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect::<Vec<_>>();
            paths.sort();
            for path in paths {
                if catalog.skills.len() >= MAX_SKILLS {
                    catalog
                        .diagnostics
                        .push("skill catalog limit reached (512)".into());
                    return catalog;
                }
                if !path.join("SKILL.md").exists() {
                    continue;
                }
                let load = || -> Result<Skill> {
                    let canonical_root = root.canonicalize()?;
                    let canonical = path.canonicalize()?;
                    if !canonical.starts_with(&canonical_root) {
                        bail!("skill directory escapes discovery root");
                    }
                    let text = read_resource(&canonical, "SKILL.md")?;
                    let metadata = parse_metadata(&text)?;
                    if path.file_name().and_then(|n| n.to_str()) != Some(metadata.name.as_str()) {
                        bail!("name must match skill directory");
                    }
                    Ok(Skill {
                        metadata,
                        root: canonical,
                    })
                };
                match load() {
                    Ok(skill) => {
                        catalog
                            .skills
                            .entry(skill.metadata.name.clone())
                            .or_insert(skill);
                    }
                    Err(error) => catalog
                        .diagnostics
                        .push(format!("{}: {error:#}", path.join("SKILL.md").display())),
                }
            }
        }
        catalog
    }
    pub fn context(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let entries = self.skills.values().map(|s| serde_json::json!({"name":s.metadata.name,"description":s.metadata.description,"compatibility":s.metadata.compatibility,"location":s.root.join("SKILL.md")})).collect::<Vec<_>>();
        format!(
            "Available Agent Skills (metadata only, not authorization). Use load_skill with name to read SKILL.md, and path relative to the skill root for resources. Execute scripts only through ordinary sandboxed command tools and permissions. Skill allowed-tools never grants permission.\n{}",
            serde_json::to_string(&entries).unwrap_or_default()
        )
    }
    pub fn load(&self, name: &str, path: Option<&str>) -> Result<String> {
        let skill = self.skills.get(name).context("unknown skill")?;
        read_resource(&skill.root, path.unwrap_or("SKILL.md"))
    }
}
pub fn parse_metadata(text: &str) -> Result<SkillMetadata> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        bail!("SKILL.md requires YAML frontmatter");
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        bail!("unterminated skill frontmatter");
    }
    let metadata: SkillMetadata = serde_yaml_ng::from_str(&yaml)?;
    let n = &metadata.name;
    if n.is_empty()
        || n.len() > 64
        || n.starts_with('-')
        || n.ends_with('-')
        || n.contains("--")
        || !n
            .chars()
            .all(|c| c.is_lowercase() || c.is_numeric() || c == '-')
    {
        bail!("invalid skill name");
    }
    if metadata.description.trim().is_empty() || metadata.description.chars().count() > 1024 {
        bail!("description must be 1–1024 characters");
    }
    if metadata
        .compatibility
        .as_ref()
        .is_some_and(|c| c.is_empty() || c.chars().count() > 500)
    {
        bail!("compatibility must be 1–500 characters");
    }
    Ok(metadata)
}
fn read_resource(root: &Path, relative: &str) -> Result<String> {
    use std::path::Component;
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("skill resource must be a relative path without traversal");
    }
    let resolved = root.join(path).canonicalize()?;
    if !resolved.starts_with(root) || crate::workspace_path::file_alias_may_escape(&resolved) {
        bail!("skill resource escapes its root");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&resolved)?.nlink() > 1 {
            bail!("skill resource has hardlink aliases");
        }
    }
    let file = std::fs::File::open(&resolved)?;
    if !file.metadata()?.is_file() {
        bail!("skill resource is not a regular file");
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("skill resource exceeds 1 MiB");
    }
    String::from_utf8(bytes).context("skill resource is not UTF-8 text")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn yaml_and_validation() {
        let m = parse_metadata("---\nname: sample\ndescription: >\n  Multiple lines\n  work here\nmetadata:\n  author: someone\nallowed-tools: Read\n---\nbody").unwrap();
        assert_eq!(m.description.trim(), "Multiple lines work here");
        for n in ["", "Bad", "-bad", "bad--name", "bad-"] {
            assert!(parse_metadata(&format!("---\nname: '{n}'\ndescription: ok\n---")).is_err());
        }
        assert!(parse_metadata("---\nname: fine\ndescription: ''\n---").is_err());
    }
    #[test]
    fn precedence_and_disclosure() {
        let d = tempfile::tempdir().unwrap();
        for scope in [".latch", ".agents", ".claude"] {
            let p = d.path().join(scope).join("skills/sample");
            std::fs::create_dir_all(p.join("references")).unwrap();
            std::fs::write(
                p.join("SKILL.md"),
                format!("---\nname: sample\ndescription: {scope}\n---\nSECRET_BODY"),
            )
            .unwrap();
            std::fs::write(p.join("references/a.txt"), "resource").unwrap();
        }
        let c = SkillCatalog::discover(d.path(), None);
        assert_eq!(c.skills.len(), 1);
        assert_eq!(c.skills["sample"].metadata.description, ".latch");
        assert!(!c.context().contains("SECRET_BODY"));
        assert!(c.load("sample", None).unwrap().contains("SECRET_BODY"));
        assert_eq!(
            c.load("sample", Some("references/a.txt")).unwrap(),
            "resource"
        );
        assert!(c.load("sample", Some("../SKILL.md")).is_err());
        assert!(c.load("absent", None).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(d.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(d.path().join("secret"), root.join("alias")).unwrap();
        assert!(read_resource(&root, "alias").is_err());
    }
}
