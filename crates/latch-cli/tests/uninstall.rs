//! Never uninstall the test runner or a developer's installed binary.
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn cancellation_and_eof_do_not_load_configuration_or_remove_the_program() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("invalid.toml");
    std::fs::write(&config, "invalid TOML [").unwrap();
    for input in ["3\n", ""] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_latch"))
            .arg("uninstall")
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("1. Remove the program only (default)"));
        assert!(text.contains("Warning: This cannot be undone."));
        assert!(text.contains("Uninstall cancelled."));
        assert!(config.exists());
        assert!(std::path::Path::new(env!("CARGO_BIN_EXE_latch")).exists());
    }
}

// Make the disposable copy unique so it cannot match a developer's installed
// standard-path binary. ELF/PE tolerate an overlay; Mach-O gets a unique signature.
fn disposable_program(root: &std::path::Path) -> std::path::PathBuf {
    let program = root.join(if cfg!(windows) {
        "latch's $test copy.exe"
    } else {
        "latch's $test copy"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_latch"), &program).unwrap();
    #[cfg(not(target_os = "macos"))]
    std::fs::OpenOptions::new()
        .append(true)
        .open(&program)
        .unwrap()
        .write_all(root.as_os_str().as_encoded_bytes())
        .unwrap();
    #[cfg(target_os = "macos")]
    assert!(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--identifier"])
            .arg(format!("test.latch.uninstall.{}", uuid::Uuid::new_v4()))
            .arg(&program)
            .status()
            .unwrap()
            .success()
    );
    program
}

#[test]
fn self_removal_of_disposable_program_preserves_configuration() {
    let root = tempfile::tempdir().unwrap();
    let program = disposable_program(root.path());
    let config = root.path().join("config.toml");
    std::fs::write(&config, "invalid TOML [").unwrap();
    let mut child = Command::new(&program)
        .args(["uninstall", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while program.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !program.exists(),
        "self-removal did not complete: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(std::fs::read_to_string(config).unwrap(), "invalid TOML [");
}

#[cfg(target_os = "linux")]
#[test]
fn program_only_default_preserves_data_and_full_uninstall_requires_confirmation() {
    for (input, removes_program, removes_data) in [
        ("\n", true, false),
        ("2\n\n", false, false),
        ("2\nDELETE\n", true, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let data = home.join(".latch");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("config.toml"), "default_mode = 'WORK'\n").unwrap();
        std::fs::write(data.join("latch.sqlite3"), "session history").unwrap();
        let custom = root.path().join("custom-state");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("secrets.toml"), "private fixture").unwrap();
        std::fs::write(
            data.join("config.toml"),
            toml::to_string(&std::collections::BTreeMap::from([(
                "state_dir",
                custom.to_str().unwrap(),
            )]))
            .unwrap(),
        )
        .unwrap();
        let legacy = home.join(".local/state/latch");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("old-history"), "old data").unwrap();
        let unrelated = root.path().join("keep.txt");
        std::fs::write(&unrelated, "keep").unwrap();
        let program = disposable_program(root.path());
        let mut child = Command::new(&program)
            .arg("uninstall")
            .env("HOME", &home)
            .env("CARGO_HOME", home.join(".cargo"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(!program.exists(), removes_program);
        assert_eq!(!data.exists(), removes_data);
        assert_eq!(!custom.exists(), removes_data);
        assert_eq!(!legacy.exists(), removes_data);
        assert!(unrelated.exists());
        if !removes_data {
            assert_eq!(
                std::fs::read_to_string(data.join("latch.sqlite3")).unwrap(),
                "session history"
            );
        }
    }
}
