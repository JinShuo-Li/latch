//! Native Windows execution through the embedded AppContainer runner.
use super::windows_runtime_tools::StagedTools;
use crate::sandbox::{Capability, SandboxProfile};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const RUNNER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/latch-windows-runner.exe"));
const COMPAT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/latch-boundary-compat.dll"));

#[derive(Debug, Clone)]
pub struct NativeRuntime {
    runtime: PathBuf,
    shell: PathBuf,
    tools: StagedTools,
}

impl NativeRuntime {
    pub fn install(root: &Path) -> Result<Self> {
        let mut digest = Sha256::new();
        digest.update(RUNNER);
        digest.update(COMPAT);
        let runtime = root.join(hex::encode(digest.finalize()));
        std::fs::create_dir_all(&runtime)?;
        materialize(&runtime, "latch-windows-runner.exe", RUNNER)?;
        materialize(&runtime, "latch-boundary-compat.dll", COMPAT)?;
        let system = std::env::var_os("SystemRoot").context("SystemRoot is required on Windows")?;
        let shell = PathBuf::from(system).join("System32").join("cmd.exe");
        ensure!(shell.is_file(), "native cmd.exe is required");
        let tools = StagedTools::discover(root);
        Ok(Self {
            runtime,
            shell,
            tools,
        })
    }

    pub fn command(&self, profile: &SandboxProfile, script: &str) -> Result<Command> {
        ensure!(
            !script.contains(char::from(0)),
            "command contains a NUL character"
        );
        self.native_command(profile, &self.shell, &format!("/d /c {script}"), &[])
    }

    pub fn fixed_command(
        &self,
        profile: &SandboxProfile,
        program: &str,
        args: &[&str],
    ) -> Result<Command> {
        let program = match program.to_ascii_lowercase().as_str() {
            "python" | "python3" | "python.exe" | "python3.exe" => self
                .tools
                .python
                .clone()
                .context("Python is not installed")?,
            "node" | "node.exe" => self.tools.node.clone().context("Node is not installed")?,
            _ => executable(program)?,
        };
        let mut reads = Vec::new();
        for arg in args {
            let path = Path::new(arg);
            if path.is_absolute() && path.is_file() {
                let path = std::fs::canonicalize(path)?;
                ensure!(
                    !path.starts_with(&profile.state_dir)
                        && !super::windows_secret_paths(&profile.home)
                            .iter()
                            .any(|secret| path.starts_with(secret)),
                    "protected file cannot be passed to a Windows subprocess"
                );
                if !path.starts_with(&profile.workspace) {
                    reads.push(native_path(&path)?);
                }
            }
        }
        let args = args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        self.native_command(profile, &program, &args, &reads)
    }

    fn native_command(
        &self,
        profile: &SandboxProfile,
        program: &Path,
        args: &str,
        reads: &[PathBuf],
    ) -> Result<Command> {
        ensure!(
            !profile.capabilities.any(&[
                Capability::PrivilegedOperation,
                Capability::DestructiveOperation
            ]),
            "privileged and system-destructive operations are denied"
        );
        ensure!(
            profile.external_roots.is_empty()
                || profile
                    .capabilities
                    .contains(Capability::ExternalFilesystemWrite),
            "external writable roots require ExternalFilesystemWrite"
        );
        let workspace = native_path(&std::fs::canonicalize(&profile.workspace)?)?;
        let mut process = Command::new(self.runtime.join("latch-windows-runner.exe"));
        // The trusted helper must not load DLLs from an untrusted current directory.
        process
            .current_dir(&self.runtime)
            .env_clear()
            .stdin(Stdio::null())
            .kill_on_drop(true);
        process
            .arg(&workspace)
            .arg(program)
            .arg(args)
            .arg(if profile.workspace_writable() {
                "write"
            } else {
                "read"
            })
            .arg("--read-root")
            .arg(&self.runtime)
            .arg("--network")
            .arg(if profile.network() { "yes" } else { "no" });
        for key in [
            "SystemRoot",
            "SystemDrive",
            "WINDIR",
            "PATH",
            "PATHEXT",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "COMSPEC",
            "LOCALAPPDATA",
            "APPDATA",
            "USERPROFILE",
            "TEMP",
            "TMP",
            "INCLUDE",
            "LIB",
            "LIBPATH",
            "VCINSTALLDIR",
            "VSINSTALLDIR",
            "VCToolsInstallDir",
            "VCToolsVersion",
            "WindowsSdkDir",
            "WindowsSDKVersion",
            "UniversalCRTSdkDir",
            "UCRTVersion",
            "RUSTUP_HOME",
            "RUSTUP_TOOLCHAIN",
            "CARGO_HOME",
            "GOPATH",
            "JAVA_HOME",
            "VIRTUAL_ENV",
            "LANG",
            "LC_ALL",
            "TZ",
        ] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = Vec::new();
        if let Some(node) = &self.tools.node {
            paths.push(node.parent().context("Node runtime")?.to_path_buf());
        }
        if let Some(python) = &self.tools.python {
            paths.push(python.parent().context("Python runtime")?.to_path_buf());
        }
        paths.extend(std::env::split_paths(&original_path));
        process.env("PATH", std::env::join_paths(paths)?);
        process.env("npm_config_cache", workspace.join(".npm-cache"));
        process
            .env("HOME", &profile.home)
            .env("LATCH_SANDBOX", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
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
        for path in reads {
            process.arg("--read-root").arg(path);
        }
        for root in &profile.external_roots {
            process
                .arg("--write-root")
                .arg(native_path(&std::fs::canonicalize(root)?)?);
        }
        if !profile.git_writable() {
            process.arg("--protect-git").arg(&workspace);
            for path in git_paths(&workspace)? {
                process.arg("--deny-write").arg(native_path(&path)?);
            }
        } else {
            for path in git_paths(&workspace)? {
                if path.is_dir() && !path.starts_with(&workspace) {
                    process.arg("--write-root").arg(native_path(&path)?);
                }
            }
        }
        if program.starts_with(&profile.home) && !program.starts_with(&self.runtime) {
            // Grant only the selected user-owned binary, never its parent tree.
            process.arg("--read-root").arg(native_path(program)?);
        }
        let invocation = format!("{} {}", program.display(), args).to_ascii_lowercase();
        if let Some(python) = &self.tools.python
            && (program == python
                || invocation
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| word == "python" || word == "python3"))
        {
            self.tools.ensure_python()?;
            let root = python.parent().context("Python runtime")?;
            process
                .arg("--read-root")
                .arg(root)
                .arg("--deny-write")
                .arg(root);
        }
        if let Some(node) = &self.tools.node
            && (program == node
                || invocation
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| word == "node" || word == "npm"))
        {
            self.tools.ensure_node()?;
            let root = node.parent().context("Node runtime")?;
            process
                .arg("--read-root")
                .arg(root)
                .arg("--deny-write")
                .arg(root);
        }
        process.arg("--deny-write").arg(&self.runtime);
        let mut secrets = super::windows_secret_paths(&profile.home);
        if profile.state_dir.exists() {
            secrets.push(profile.state_dir.clone());
        }
        for path in secrets {
            process.arg("--deny").arg(path);
        }
        Ok(process)
    }
}

pub(super) fn materialize(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(name);
    if !path.exists() {
        let mut temporary = tempfile::NamedTempFile::new_in(root)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error).context("install native runtime asset"),
        }
    }
    ensure!(
        std::fs::read(&path)? == bytes,
        "native runtime integrity check failed: {}",
        path.display()
    );
    Ok(())
}

fn native_path(path: &Path) -> Result<PathBuf> {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(drive) => {
                let mut result = PathBuf::from(format!("{}:{}", char::from(drive), char::from(92)));
                for component in components {
                    if !matches!(component, Component::RootDir) {
                        result.push(component);
                    }
                }
                Ok(result)
            }
            Prefix::Disk(_) => Ok(path.to_path_buf()),
            _ => anyhow::bail!("Windows execution requires a local NTFS drive"),
        },
        _ => anyhow::bail!("Windows execution requires an absolute local path"),
    }
}

fn executable(program: &str) -> Result<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() {
        ensure!(path.is_file(), "program does not exist: {program}");
        return Ok(path.to_path_buf());
    }
    ensure!(
        path.components().count() == 1,
        "fixed Windows program must be a name or absolute path"
    );
    let name = if path.extension().is_some() {
        program.to_owned()
    } else {
        format!("{program}.exe")
    };
    let search = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&search)
        .filter(|path| path.is_absolute())
        .map(|directory| directory.join(&name))
        .find(|path| path.is_file())
        .with_context(|| format!("native Windows program {program} is required on PATH"))
}

fn quote(value: &str) -> Result<String> {
    ensure!(
        !value.contains(char::from(0)),
        "argument contains a NUL character"
    );
    let slash = char::from(92);
    let quote = char::from(34);
    let mut result = String::from(quote);
    let mut pending = 0;
    for character in value.chars() {
        if character == slash {
            pending += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            slash,
            if character == quote {
                pending * 2 + 1
            } else {
                pending
            },
        ));
        result.push(character);
        pending = 0;
    }
    result.extend(std::iter::repeat_n(slash, pending * 2));
    result.push(quote);
    Ok(result)
}

fn git_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    use std::os::windows::fs::MetadataExt;
    let mut directories = vec![workspace.to_path_buf()];
    let mut markers = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git")
            {
                ensure!(
                    std::fs::symlink_metadata(&path)?.file_attributes() & 0x400 == 0,
                    "Git marker cannot be a reparse point: {}",
                    path.display()
                );
                markers.push(path);
            } else if entry.file_type()?.is_dir() {
                directories.push(path);
            }
        }
    }
    let mut paths = std::collections::BTreeSet::new();
    for marker in markers {
        let git = if marker.is_file() {
            let contents = std::fs::read_to_string(&marker)?;
            let target = contents
                .trim()
                .strip_prefix("gitdir: ")
                .context("invalid .git worktree marker")?;
            paths.insert(std::fs::canonicalize(&marker)?);
            std::fs::canonicalize(marker.parent().unwrap().join(target))?
        } else {
            std::fs::canonicalize(&marker)?
        };
        paths.insert(git.clone());
        if git.join("commondir").is_file() {
            let common = std::fs::read_to_string(git.join("commondir"))?;
            paths.insert(std::fs::canonicalize(git.join(common.trim()))?);
        }
    }
    Ok(paths.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CapabilitySet;

    #[test]
    fn nested_git_and_worktree_common_dir_are_discovered() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let nested = workspace.join("nested");
        let common = root.path().join("common");
        let gitdir = common.join("worktrees").join("nested");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(nested.join(".git"), format!("gitdir: {}", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..").unwrap();
        let discovered = git_paths(&workspace).unwrap();
        for expected in [workspace.join(".git"), nested.join(".git"), gitdir, common] {
            assert!(discovered.contains(&std::fs::canonicalize(expected).unwrap()));
        }
    }

    #[tokio::test]
    async fn native_shell_and_fixed_git_use_embedded_boundary() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        let home = root.path().join("home");
        for path in [&workspace, &state, &home] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(state.join("secrets.toml"), "fixture secret").unwrap();
        std::fs::write(workspace.join("ordinary.txt"), "workspace contents").unwrap();
        let runtime = NativeRuntime::install(&root.path().join("runtime")).unwrap();
        let profile = SandboxProfile::new(
            workspace.clone(),
            home.clone(),
            state.clone(),
            CapabilitySet::new(),
        );
        let output = runtime
            .command(&profile, "type ordinary.txt")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={} workspace={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            workspace.display()
        );
        assert_eq!(output.stdout, b"workspace contents");
        for script in ["dir /b ordinary.txt", "dir /b"] {
            let listed = runtime
                .command(&profile, script)
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(
                listed.status.success(),
                "{script} status={:?}, out={}, err={}",
                listed.status,
                String::from_utf8_lossy(&listed.stdout),
                String::from_utf8_lossy(&listed.stderr)
            );
        }
        let output = runtime
            .command(&profile, "echo blocked>ordinary.txt")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            std::fs::read(workspace.join("ordinary.txt")).unwrap(),
            b"workspace contents"
        );
        std::fs::write(workspace.join("good.txt"), "good").unwrap();
        let checked = runtime
            .command(&profile, "type good.txt | findstr good >nul")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            checked.status.success(),
            "findstr status={:?}, out={}, err={}",
            checked.status,
            String::from_utf8_lossy(&checked.stdout),
            String::from_utf8_lossy(&checked.stderr)
        );
        let output = runtime
            .fixed_command(&profile, "git", &["--version"])
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("git version "));
        if runtime.tools.python.is_some() {
            let output = runtime
                .fixed_command(&profile, "python3", &["-c", "print('python-ready')"])
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "python status={:?}, out={}, err={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"python-ready\r\n");
        }
    }
}
