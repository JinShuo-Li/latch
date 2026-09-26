//! Candidate native backend. Production detection remains fail closed until
//! the complete security and lifecycle gate passes.
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
        Ok(Self { runtime, shell })
    }

    pub fn command(&self, profile: &SandboxProfile, script: &str) -> Result<Command> {
        ensure!(
            !script.contains(char::from(0)),
            "command contains a NUL character"
        );
        self.native_command(profile, &self.shell, &format!("/d /s /c {script}"))
    }

    pub fn fixed_command(
        &self,
        profile: &SandboxProfile,
        program: &str,
        args: &[&str],
    ) -> Result<Command> {
        let program = executable(program)?;
        let args = args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        self.native_command(profile, &program, &args)
    }

    fn native_command(
        &self,
        profile: &SandboxProfile,
        program: &Path,
        args: &str,
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
        process
            .env("HOME", &profile.home)
            .env("LATCH_SANDBOX", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        for root in [profile.home.join(".rustup"), profile.home.join(".cargo")] {
            if root.is_dir() {
                process.arg("--read-root").arg(root);
            }
        }
        for key in [
            "RUSTUP_HOME",
            "CARGO_HOME",
            "GOPATH",
            "JAVA_HOME",
            "VIRTUAL_ENV",
        ] {
            if let Some(root) = std::env::var_os(key)
                .map(PathBuf::from)
                .filter(|path| path.is_dir())
            {
                process.arg("--read-root").arg(root);
            }
        }
        for root in &profile.external_roots {
            process
                .arg("--write-root")
                .arg(std::fs::canonicalize(root)?);
        }
        if !profile.git_writable() {
            process.arg("--protect-git").arg(&workspace);
            for path in git_paths(&workspace)? {
                process.arg("--deny-write").arg(path);
            }
        } else {
            for path in git_paths(&workspace)? {
                if path.is_dir() && !path.starts_with(&workspace) {
                    process.arg("--write-root").arg(path);
                }
            }
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

fn materialize(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
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
    let marker = workspace.join(".git");
    if !marker.exists() {
        return Ok(Vec::new());
    }
    let mut paths = vec![std::fs::canonicalize(&marker)?];
    let git = if marker.is_file() {
        let contents = std::fs::read_to_string(&marker)?;
        let target = contents
            .trim()
            .strip_prefix("gitdir: ")
            .context("invalid .git worktree marker")?;
        let target = std::fs::canonicalize(workspace.join(target))?;
        paths.push(target.clone());
        target
    } else {
        paths[0].clone()
    };
    if git.join("commondir").is_file() {
        let common = std::fs::read_to_string(git.join("commondir"))?;
        paths.push(std::fs::canonicalize(git.join(common.trim()))?);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CapabilitySet;

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
        let profile = SandboxProfile::new(workspace.clone(), home, state, CapabilitySet::new());
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
    }
}
