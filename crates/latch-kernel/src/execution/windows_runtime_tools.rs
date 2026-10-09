use super::windows_runtime::materialize;
use crate::sandbox::SandboxProfile;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub(super) struct StagedTools {
    pub python: Option<PathBuf>,
    pub node: Option<PathBuf>,
    pub rg: Option<PathBuf>,
    pub pwsh: Option<PathBuf>,
    python_source: Option<PathBuf>,
    node_source: Option<PathBuf>,
    rg_source: Option<PathBuf>,
    pwsh_source: Option<PathBuf>,
}

static STAGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl StagedTools {
    pub fn discover(root: &Path) -> Self {
        let python_source = on_path("python.exe");
        let node_source = on_path("node.exe");
        let rg_source = on_path("rg.exe").and_then(|path| standalone_ripgrep(&path));
        let pwsh_source = powershell_source();
        Self {
            python: python_source
                .as_ref()
                .map(|_| root.join("tool-python-zip-v3/python.exe")),
            node: node_source
                .as_ref()
                .map(|_| root.join("tool-node/node.exe")),
            rg: rg_source.as_ref().map(|_| root.join("tool-rg/rg.exe")),
            pwsh: pwsh_source.as_ref().map(|source| {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(source.to_string_lossy().as_bytes());
                if let Some(parent) = source.parent()
                    && let Ok(host) = std::fs::read(parent.join("pwsh.dll"))
                {
                    hasher.update(host);
                }
                let digest = hasher.finalize();
                root.join(format!("tool-pwsh-{:x}/pwsh.exe", digest))
            }),
            python_source,
            node_source,
            rg_source,
            pwsh_source,
        }
    }

    pub fn fixed_program(&self, program: &str) -> Result<Option<PathBuf>> {
        match program.to_ascii_lowercase().as_str() {
            "python" | "python3" | "python.exe" | "python3.exe" => Ok(Some(
                self.python.clone().context("Python is not installed")?,
            )),
            "node" | "node.exe" => Ok(Some(self.node.clone().context("Node is not installed")?)),
            "pwsh" | "pwsh.exe" => Ok(Some(self.pwsh.clone().context(
                "PowerShell 7 is not installed; install a native PowerShell 7 runtime",
            )?)),
            "rg" | "rg.exe" => Ok(Some(self.rg.clone().context(
                "ripgrep is not installed or its package lacks a standalone rg.exe",
            )?)),
            _ => Ok(None),
        }
    }

    pub fn path_entries(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        if let Some(node) = &self.node {
            paths.push(node.parent().context("Node runtime")?.to_path_buf());
        }
        if let Some(python) = &self.python {
            paths.push(python.parent().context("Python runtime")?.to_path_buf());
        }
        if let Some(rg) = &self.rg {
            paths.push(rg.parent().context("ripgrep runtime")?.to_path_buf());
        }
        if let Some(pwsh) = &self.pwsh {
            paths.push(pwsh.parent().context("PowerShell runtime")?.to_path_buf());
        }
        Ok(paths)
    }

    pub fn grant_rust_roots(
        &self,
        process: &mut Command,
        profile: &SandboxProfile,
        program: &Path,
        args: &str,
    ) -> Result<()> {
        // A user toolchain home can contain hundreds of thousands of files
        // and credentials. Never grant or journal the entire home for an
        // unrelated shell/Git invocation. Tool-specific roots are added below.
        let rust_tool = ["cargo", "rustc", "rustup", "rustdoc"].iter().any(|name| {
            program
                .file_stem()
                .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
                || args
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|word| word.eq_ignore_ascii_case(name))
        });
        if rust_tool {
            let rustup = std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| profile.home.join(".rustup"));
            let cargo = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| profile.home.join(".cargo"));
            for root in [cargo.join("bin"), rustup.join("settings.toml")] {
                if root.exists() {
                    process.arg("--read-root").arg(root);
                }
            }
            let toolchains = rustup.join("toolchains");
            if toolchains.is_dir() {
                for entry in std::fs::read_dir(toolchains)? {
                    let path = entry?.path();
                    if path.is_dir() {
                        for component in ["bin", "lib"] {
                            let root = path.join(component);
                            if root.is_dir() {
                                process.arg("--read-root").arg(root);
                            }
                        }
                    }
                }
            }
            if profile.workspace.join("Cargo.toml").is_file()
                && !args.contains("--version")
                && !args.contains("-V")
            {
                let registry = cargo.join("registry");
                if registry.is_dir() {
                    process.arg("--read-root").arg(registry);
                }
            }
        }
        Ok(())
    }

    pub fn grant_staged_roots(
        &self,
        process: &mut Command,
        program: &Path,
        args: &str,
    ) -> Result<()> {
        let invocation = format!("{} {}", program.display(), args).to_ascii_lowercase();
        if let Some(pwsh) = &self.pwsh
            && (program == pwsh
                || invocation
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| word == "pwsh"))
        {
            self.ensure_pwsh()?;
            let root = pwsh.parent().context("PowerShell runtime")?;
            process
                .arg("--read-root")
                .arg(root)
                .arg("--deny-write")
                .arg(root)
                .env("PSModulePath", root.join("Modules"));
        }
        if let Some(python) = &self.python
            && (program == python
                || invocation
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| word == "python" || word == "python3"))
        {
            self.ensure_python()?;
            let root = python.parent().context("Python runtime")?;
            process
                .arg("--read-root")
                .arg(root)
                .arg("--deny-write")
                .arg(root);
        }
        if let Some(node) = &self.node
            && (program == node
                || invocation
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| word == "node" || word == "npm"))
        {
            self.ensure_node()?;
            let root = node.parent().context("Node runtime")?;
            process
                .arg("--read-root")
                .arg(root)
                .arg("--deny-write")
                .arg(root);
        }
        if let Some(rg) = &self.rg
            && (program == rg
                || invocation
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| word == "rg"))
        {
            self.ensure_rg()?;
            let root = rg.parent().context("ripgrep runtime")?;
            process
                .arg("--read-root")
                .arg(root)
                .arg("--deny-write")
                .arg(root);
        }
        Ok(())
    }

    pub fn ensure_rg(&self) -> Result<()> {
        let Some(source) = &self.rg_source else {
            return Ok(());
        };
        let target = self.rg.as_ref().context("ripgrep target")?;
        let _lock = STAGE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("tool staging lock poisoned"))?;
        stage_file(source, target)
    }

    fn ensure_pwsh(&self) -> Result<()> {
        let source = self.pwsh_source.as_ref().context("PowerShell source")?;
        let root = self
            .pwsh
            .as_ref()
            .context("PowerShell target")?
            .parent()
            .context("PowerShell parent")?;
        let _lock = STAGE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("tool staging lock poisoned"))?;
        // Include the managed host identity so an in-place runtime upgrade
        // cannot reuse the completion marker for an older staged installation.
        use sha2::{Digest, Sha256};
        let source_root = source.parent().context("PowerShell source parent")?;
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(std::fs::read(source_root.join("pwsh.dll"))?)
        );
        if std::fs::read_to_string(root.join(".complete"))
            .ok()
            .as_deref()
            == Some(&fingerprint)
        {
            return Ok(());
        }
        stage_tree(source_root, root)?;
        materialize(root, ".complete", fingerprint.as_bytes())
    }

    pub fn ensure_python(&self) -> Result<()> {
        let Some(source) = &self.python_source else {
            return Ok(());
        };
        let target = self
            .python
            .as_ref()
            .context("Python target")?
            .parent()
            .context("Python parent")?;
        let _lock = STAGE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("tool staging lock poisoned"))?;
        if target.join(".complete").is_file() {
            return Ok(());
        }
        stage_file(source, &target.join("python.exe"))?;
        stage_file(source, &target.join("python3.exe"))?;
        let source_root = source.parent().context("Python installation parent")?;
        let mut version = None;
        for entry in std::fs::read_dir(source_root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if (name.starts_with("python3") && name.ends_with(".dll"))
                || name.starts_with("vcruntime140") && name.ends_with(".dll")
                || name == "zlib.dll"
                || name == "ucrtbase.dll"
            {
                stage_file(&entry.path(), &target.join(entry.file_name()))?;
                if name.starts_with("python3") && name != "python3.dll" {
                    version = Some(name.replace(".dll", "._pth"));
                }
            }
        }
        let version = version.context("Python version DLL is required")?;
        let lib = source_root.join("Lib");
        ensure!(lib.is_dir(), "Python standard library is required");
        let archive = python_stdlib_zip(&lib)?;
        materialize(target, "python-stdlib.zip", &archive)?;
        let dlls = source_root.join("DLLs");
        if dlls.is_dir() {
            stage_tree(&dlls, &target.join("DLLs"))?;
        }
        // `._pth` isolates imports from the host installation. Restore the
        // ordinary Python `-m` behavior for the approved workspace only.
        materialize(
            target,
            "sitecustomize.py",
            b"import os\nimport sys\nsys.path.insert(0, os.getcwd())\n",
        )?;
        materialize(
            target,
            &version,
            b"python-stdlib.zip\r\nDLLs\r\n.\r\nimport site\r\n",
        )?;
        materialize(target, ".complete", b"python core runtime zip v3")
    }

    pub fn ensure_node(&self) -> Result<()> {
        let Some(source) = &self.node_source else {
            return Ok(());
        };
        let target = self
            .node
            .as_ref()
            .context("Node target")?
            .parent()
            .context("Node parent")?;
        let _lock = STAGE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("tool staging lock poisoned"))?;
        if target.join(".complete").is_file() {
            return Ok(());
        }
        stage_file(source, &target.join("node.exe"))?;
        let source_root = source.parent().context("Node installation parent")?;
        let npm = source_root.join("npm.cmd");
        if npm.is_file() {
            stage_file(&npm, &target.join("npm.cmd"))?;
            let modules = source_root.join("node_modules/npm");
            if modules.is_dir() {
                stage_tree(&modules, &target.join("node_modules/npm"))?;
            }
        }
        materialize(target, ".complete", b"node core runtime v1")
    }
}

pub(super) fn has_ripgrep() -> bool {
    on_path("rg.exe")
        .and_then(|path| standalone_ripgrep(&path))
        .is_some()
}
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file() && !path.to_string_lossy().contains("WindowsApps"))
}

fn powershell_source() -> Option<PathBuf> {
    let mut candidates = on_path("pwsh.exe").into_iter().collect::<Vec<_>>();
    if let Some(modules) = std::env::var_os("PSModulePath") {
        candidates.extend(
            std::env::split_paths(&modules)
                .filter_map(|directory| directory.parent().map(|parent| parent.join("pwsh.exe"))),
        );
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(program_files).join("PowerShell/7/pwsh.exe"));
    }
    candidates.into_iter().find(|path| {
        path.is_file()
            && path
                .parent()
                .is_some_and(|parent| parent.join("pwsh.dll").is_file())
    })
}

// Chocolatey exposes a launcher shim on PATH. A restricted AppContainer cannot
// rely on that shim starting the package executable from machine-owned storage.
// Stage the standalone executable itself into Latch's user-owned runtime.
fn standalone_ripgrep(path: &Path) -> Option<PathBuf> {
    let bin = path.parent()?;
    let package = bin.parent()?.join("lib/ripgrep");
    if bin.file_name()?.eq_ignore_ascii_case("bin") && package.is_dir() {
        return find_rg(&package, 3);
    }
    Some(path.to_path_buf())
}

fn find_rg(directory: &Path, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let mut entries = std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_name().eq_ignore_ascii_case("rg.exe") && path.is_file() {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_rg(&path, depth - 1)
        {
            return Some(found);
        }
    }
    None
}

fn stage_file(source: &Path, target: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
        "tool asset is a reparse point: {}",
        source.display()
    );
    let parent = target.parent().context("tool asset parent")?;
    std::fs::create_dir_all(parent)?;
    materialize(
        parent,
        target
            .file_name()
            .context("tool asset name")?
            .to_str()
            .context("tool asset UTF-8 name")?,
        &std::fs::read(source)?,
    )
}

fn stage_tree(source: &Path, target: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "site-packages" || name == "__pycache__" {
            continue;
        }
        let source = entry.path();
        let destination = target.join(name);
        let metadata = std::fs::symlink_metadata(&source)?;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "tool directory contains a reparse point: {}",
            source.display()
        );
        if metadata.is_dir() {
            stage_tree(&source, &destination)?;
        } else if metadata.is_file() {
            stage_file(&source, &destination)?;
        } else {
            anyhow::bail!("unsupported tool asset: {}", source.display());
        }
    }
    Ok(())
}

// ZIP_STORED is sufficient for Python's zipimport and keeps the trusted
// staging path independent of an external compressor or an unsandboxed tool.
// One archive replaces thousands of per-call NTFS ACL mutations and journal
// entries while remaining a read-only, call-scoped grant.
fn python_stdlib_zip(root: &Path) -> Result<Vec<u8>> {
    use std::os::windows::fs::MetadataExt;
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Python library contains a reparse point: {}",
                path.display()
            );
            let name = entry.file_name();
            if name == "site-packages" || name == "__pycache__" {
                continue;
            }
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                files.push(path);
            } else {
                anyhow::bail!("unsupported Python library asset: {}", path.display());
            }
        }
    }
    files.sort();
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for path in &files {
        let relative = path.strip_prefix(root)?;
        let name = relative.to_string_lossy().replace('\\', "/");
        let name = name.as_bytes();
        let bytes = std::fs::read(path)?;
        let size = u32::try_from(bytes.len()).context("Python library file exceeds ZIP32")?;
        let offset = u32::try_from(archive.len()).context("Python ZIP exceeds ZIP32")?;
        let crc = crc32(&bytes);
        let name_len = u16::try_from(name.len()).context("Python ZIP path too long")?;
        zip_u32(&mut archive, 0x0403_4b50);
        zip_u16(&mut archive, 20);
        zip_u16(&mut archive, 0x0800);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u32(&mut archive, crc);
        zip_u32(&mut archive, size);
        zip_u32(&mut archive, size);
        zip_u16(&mut archive, name_len);
        zip_u16(&mut archive, 0);
        archive.extend_from_slice(name);
        archive.extend_from_slice(&bytes);

        zip_u32(&mut central, 0x0201_4b50);
        zip_u16(&mut central, 20);
        zip_u16(&mut central, 20);
        zip_u16(&mut central, 0x0800);
        zip_u16(&mut central, 0);
        zip_u16(&mut central, 0);
        zip_u16(&mut central, 0);
        zip_u32(&mut central, crc);
        zip_u32(&mut central, size);
        zip_u32(&mut central, size);
        zip_u16(&mut central, name_len);
        zip_u16(&mut central, 0);
        zip_u16(&mut central, 0);
        zip_u16(&mut central, 0);
        zip_u16(&mut central, 0);
        zip_u32(&mut central, 0);
        zip_u32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let count = u16::try_from(files.len()).context("too many Python ZIP entries")?;
    let central_offset = u32::try_from(archive.len()).context("Python ZIP exceeds ZIP32")?;
    let central_size = u32::try_from(central.len()).context("Python ZIP exceeds ZIP32")?;
    archive.extend_from_slice(&central);
    zip_u32(&mut archive, 0x0605_4b50);
    zip_u16(&mut archive, 0);
    zip_u16(&mut archive, 0);
    zip_u16(&mut archive, count);
    zip_u16(&mut archive, count);
    zip_u32(&mut archive, central_size);
    zip_u32(&mut archive, central_offset);
    zip_u16(&mut archive, 0);
    Ok(archive)
}

fn zip_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn zip_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::standalone_ripgrep;

    #[test]
    fn chocolatey_shim_resolves_to_standalone_ripgrep() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let tools = root.path().join("lib/ripgrep/tools/ripgrep-package");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let shim = bin.join("rg.exe");
        let actual = tools.join("rg.exe");
        std::fs::write(&shim, b"shim").unwrap();
        std::fs::write(&actual, b"actual").unwrap();
        assert_eq!(standalone_ripgrep(&shim), Some(actual));
    }
}
