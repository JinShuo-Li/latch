//! Human-invoked host maintenance; never exposed as an agent tool.
use anyhow::{Context, Result, bail};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

use super::command::Args;

pub fn execute(args: &Args) -> Result<()> {
    let executable = std::env::current_exe().context("locate the running Latch executable")?;
    let binaries = binary_copies(&executable)?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    writeln!(
        output,
        "Uninstall Latch\n\n1. Remove the program only (default)\n2. Remove the program, data, and configuration (Warning: This cannot be undone.)\n3. Cancel\n\nProgram files:"
    )?;
    for path in &binaries {
        writeln!(output, "  {}", path.display())?;
    }
    writeln!(output, "\nClose other Latch sessions before continuing.")?;
    match choose(&mut input, &mut output)? {
        3 => {
            writeln!(output, "Uninstall cancelled.")?;
            return Ok(());
        }
        2 => {
            let paths = data_paths(args.config.as_deref())?;
            if paths.iter().any(|root| {
                binaries.iter().any(|binary| {
                    binary.canonicalize().is_ok_and(|binary| {
                        root.canonicalize()
                            .is_ok_and(|root| binary.starts_with(root))
                    })
                })
            }) {
                bail!(
                    "program files are inside data storage; choose option 1, then remove that storage manually"
                );
            }
            writeln!(
                output,
                "\nPermanently delete these data and configuration paths:"
            )?;
            for path in &paths {
                let suffix = if is_alias(&std::fs::symlink_metadata(path)?) {
                    " (link only; target contents will be kept)"
                } else {
                    ""
                };
                writeln!(output, "  {}{suffix}", path.display())?;
            }
            writeln!(
                output,
                "Warning: This cannot be undone. Local credentials and session history will be lost."
            )?;
            write!(output, "Type DELETE to confirm, or press Enter to cancel: ")?;
            output.flush()?;
            let mut line = String::new();
            if input.read_line(&mut line)? == 0 || line.trim() != "DELETE" {
                writeln!(output, "Uninstall cancelled.")?;
                return Ok(());
            }
            if data_paths(args.config.as_deref())? != paths {
                bail!("data locations changed during confirmation; run latch uninstall again");
            }
            for path in paths {
                remove_path(&path).with_context(|| {
                    format!(
                        "remove {}; the program has not been removed",
                        path.display()
                    )
                })?;
            }
        }
        _ => {}
    }
    remove_binaries(&binaries)?;
    #[cfg(not(windows))]
    writeln!(output, "Latch uninstalled.")?;
    #[cfg(windows)]
    writeln!(
        output,
        "Uninstall scheduled. Program files will be removed after this process exits."
    )?;
    Ok(())
}

fn choose(input: &mut impl BufRead, output: &mut impl Write) -> Result<u8> {
    loop {
        write!(output, "Choose an option [1]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(3);
        }
        match line.trim() {
            "" | "1" => return Ok(1),
            "2" => return Ok(2),
            "3" => return Ok(3),
            _ => writeln!(output, "Please enter 1, 2, or 3.")?,
        }
    }
}

fn same_file_contents(left: &Path, right: &Path) -> Result<bool> {
    let mut a = std::fs::File::open(left)?;
    let mut b = std::fs::File::open(right)?;
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let mut ab = [0; 65536];
    let mut bb = [0; 65536];
    loop {
        let count = a.read(&mut ab)?;
        if count == 0 {
            return Ok(true);
        }
        b.read_exact(&mut bb[..count])?;
        if ab[..count] != bb[..count] {
            return Ok(false);
        }
    }
}

fn binary_copies(executable: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![executable.to_path_buf()];
    let mut identities = vec![executable.canonicalize()?];
    let home = dirs::home_dir().context("cannot resolve the home directory")?;
    let name = if cfg!(windows) { "latch.exe" } else { "latch" };
    let mut candidates = vec![
        home.join(".local/bin").join(name),
        home.join(".cargo/bin").join(name),
    ];
    if let Some(root) = std::env::var_os("CARGO_HOME") {
        candidates.push(PathBuf::from(root).join("bin").join(name));
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(root).join("Programs/Latch/bin/latch.exe"));
    }
    for path in candidates {
        if paths.contains(&path) {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(&path)
            && !is_alias(&meta)
            && meta.is_file()
            && same_file_contents(executable, &path).unwrap_or(false)
        {
            let identity = path.canonicalize()?;
            if !identities.contains(&identity) {
                identities.push(identity);
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

fn is_alias(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

// Resolve ancestors once while preserving a final symlink as a link, never its target.
fn existing_path(path: &Path) -> Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let name = path.file_name().context("refusing a filesystem root")?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            Ok(Some(parent.canonicalize()?.join(name)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[derive(serde::Deserialize)]
struct StorageConfig {
    state_dir: Option<PathBuf>,
}

fn configured_state(config: &Path) -> Result<Option<PathBuf>> {
    let Some(path) = existing_path(config)? else {
        return Ok(None);
    };
    if is_alias(&std::fs::symlink_metadata(&path)?) {
        return Ok(None);
    }
    let mut text = String::new();
    std::fs::File::open(&path)?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    if text.len() > 1024 * 1024 {
        bail!("configuration is too large; remove custom storage manually or choose option 1");
    }
    let config: StorageConfig = toml::from_str(&text).map_err(|_| {
        anyhow::anyhow!(
            "cannot determine storage from {}; repair the configuration or choose option 1",
            path.display()
        )
    })?;
    if config
        .state_dir
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        bail!("empty custom state directory; choose option 1 and remove Latch files manually");
    }
    Ok(config.state_dir)
}

const STORAGE_ENTRIES: &[&str] = &[
    "config.toml",
    "config.toml.migrated-backup",
    "secrets.toml",
    "secrets.toml.migrated-backup",
    "latch.sqlite3",
    "latch.sqlite3-wal",
    "latch.sqlite3-shm",
    "chatgpt-auth.toml",
    "chatgpt-auth.lock",
    "migration.toml",
    "artifacts",
    "cache",
    "runtime",
    "skills",
];

fn validate_custom_state(path: &Path) -> Result<()> {
    if is_alias(&std::fs::symlink_metadata(path)?) {
        return Ok(());
    }
    let home = dirs::home_dir()
        .context("cannot resolve the home directory")?
        .canonicalize()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let scope = path.canonicalize()?;
    if home.starts_with(&scope) || cwd.starts_with(&scope) {
        bail!(
            "refusing to delete shared state directory {}; choose option 1 and remove Latch files manually",
            path.display()
        );
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if !STORAGE_ENTRIES
            .iter()
            .any(|name| entry.file_name() == *name)
        {
            bail!(
                "custom state directory {} contains unrecognized files; choose option 1 and remove Latch files manually",
                path.display()
            );
        }
    }
    Ok(())
}

fn data_paths(config_override: Option<&Path>) -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir().context("cannot resolve the home directory")?;
    let config_root = dirs::config_dir()
        .unwrap_or_else(|| home.join(".config"))
        .join("latch");
    let state_root = dirs::state_dir()
        .unwrap_or_else(|| home.join(".local/state"))
        .join("latch");
    let mut roots = vec![home.join(".latch"), config_root.clone(), state_root];
    if let Some(cache) = dirs::cache_dir() {
        roots.push(cache.join("latch"));
    }
    let mut configs = vec![
        home.join(".latch/config.toml"),
        config_root.join("config.toml"),
    ];
    if let Some(path) = config_override {
        configs.push(path.to_path_buf());
    }
    let mut custom = Vec::new();
    for config in &configs {
        for path in [
            config.clone(),
            config.with_extension("toml.migrated-backup"),
        ] {
            if let Some(state) = configured_state(&path)? {
                custom.push(state);
            }
        }
    }
    let mut paths = Vec::new();
    for root in roots.drain(..) {
        if let Some(path) = existing_path(&root)? {
            paths.push(path);
        }
    }
    for root in custom {
        if let Some(path) = existing_path(&root)?
            && !paths.contains(&path)
        {
            validate_custom_state(&path)?;
            paths.push(path);
        }
    }
    if let Some(config) = config_override {
        for file in [
            config.to_path_buf(),
            config.with_extension("toml.migrated-backup"),
        ] {
            if let Some(path) = existing_path(&file)? {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    let snapshot = paths.clone();
    paths.retain(|path| {
        !snapshot
            .iter()
            .any(|other| other != path && path.starts_with(other))
    });
    Ok(paths)
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if is_alias(&meta) {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x10 != 0 {
                std::fs::remove_dir(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }
        #[cfg(not(windows))]
        std::fs::remove_file(path)?;
    } else if meta.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn remove_binaries(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        std::fs::remove_file(path).with_context(|| {
            format!(
                "remove program {}; check directory permissions",
                path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(windows)]
fn remove_binaries(paths: &[PathBuf]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let shell = PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is required")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let literals = paths
        .iter()
        .map(|path| {
            path.to_str()
                .map(|path| format!("'{}'", path.replace('\'', "''")))
                .context("program path is not valid Unicode")
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let script = format!(
        "Wait-Process -Id {} -Timeout 30 -ErrorAction SilentlyContinue; foreach ($p in @({literals})) {{ for ($i=0; $i -lt 100; $i++) {{ try {{ Remove-Item -LiteralPath $p -Force -ErrorAction Stop; break }} catch {{ Start-Sleep -Milliseconds 100 }} }} }}",
        std::process::id()
    );
    Command::new(shell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .context("schedule program removal")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_defaults_only_on_enter_and_cancels_on_eof() {
        for (text, expected) in [
            ("\n", 1),
            ("1\n", 1),
            ("2\n", 2),
            ("3\n", 3),
            ("", 3),
            ("bad\n3\n", 3),
        ] {
            assert_eq!(
                choose(&mut std::io::Cursor::new(text), &mut Vec::new()).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn custom_storage_rejects_unrelated_files_before_deletion() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("latch.sqlite3"), "history").unwrap();
        assert!(validate_custom_state(root.path()).is_ok());
        std::fs::write(root.path().join("project.rs"), "keep").unwrap();
        assert!(validate_custom_state(root.path()).is_err());
        assert!(validate_custom_state(&std::env::current_dir().unwrap()).is_err());
        assert!(root.path().join("project.rs").exists());
    }
    #[cfg(unix)]
    #[test]
    fn deleting_storage_never_follows_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("keep"), "data").unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("linked")).unwrap();
        remove_path(root.path()).unwrap();
        assert!(external.path().join("keep").exists());
    }
}
