//! Build discovery tests run on every host without requiring a Windows SDK.
#[path = "../build_support/windows.rs"]
mod windows;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn executable(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"fixture").unwrap();
    path
}

#[test]
fn explicit_cmake_overrides_path_and_handles_spaces() {
    let root = tempfile::tempdir().unwrap();
    let explicit = executable(root.path(), "custom & tools/cmake.exe");
    let fallback = executable(root.path(), "path/cmake.exe");
    let path = std::env::join_paths([fallback.parent().unwrap()]).unwrap();
    assert_eq!(
        windows::find_cmake(Some(explicit.as_os_str()), Some(&path), Path::new("cl.exe")).unwrap(),
        explicit
    );
}

#[test]
fn invalid_explicit_cmake_does_not_silently_fall_back() {
    let root = tempfile::tempdir().unwrap();
    let fallback = executable(root.path(), "path/cmake.exe");
    let path = std::env::join_paths([fallback.parent().unwrap()]).unwrap();
    let missing = root.path().join("missing/cmake.exe");
    let error = windows::find_cmake(Some(missing.as_os_str()), Some(&path), Path::new("cl.exe"))
        .unwrap_err();
    assert!(error.contains("CMAKE="));
    assert!(error.contains("does not name an existing executable"));
}

#[test]
fn cmake_on_path_takes_precedence_over_visual_studio_bundle() {
    let root = tempfile::tempdir().unwrap();
    let compiler = executable(
        root.path(),
        "unusual VS/VC/Tools/MSVC/version/bin/Hostx64/x64/cl.exe",
    );
    executable(
        root.path(),
        "unusual VS/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe",
    );
    let cmake = executable(root.path(), "path/cmake.exe");
    let path = std::env::join_paths([cmake.parent().unwrap()]).unwrap();
    assert_eq!(
        windows::find_cmake(None, Some(&path), &compiler).unwrap(),
        cmake
    );
}

#[test]
fn bundled_cmake_is_relative_to_discovered_compiler_not_a_fixed_vs_version() {
    let root = tempfile::tempdir().unwrap();
    let compiler = executable(
        root.path(),
        "custom VS Preview/VC/Tools/MSVC/version/bin/Hostarm64/arm64/cl.exe",
    );
    let cmake = executable(
        root.path(),
        "custom VS Preview/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe",
    );
    assert_eq!(windows::find_cmake(None, None, &compiler).unwrap(), cmake);
}

#[test]
fn bare_executable_names_allow_the_windows_exe_suffix() {
    let root = tempfile::tempdir().unwrap();
    let cmake = executable(root.path(), "tools/cmake.exe");
    let path = std::env::join_paths([cmake.parent().unwrap()]).unwrap();
    assert_eq!(
        windows::find_executable(Path::new("cmake"), Some(&path)).unwrap(),
        cmake
    );
    assert_eq!(
        windows::find_executable(&cmake.with_extension(""), None).unwrap(),
        cmake
    );
}

#[test]
fn missing_cmake_reports_install_and_override_options() {
    let root = tempfile::tempdir().unwrap();
    let error = windows::find_cmake(None, None, &root.path().join("cl.exe")).unwrap_err();
    assert!(error.contains("CMake 3.25"));
    assert!(error.contains("PATH"));
    assert!(error.contains("CMAKE"));
}

#[test]
fn make_generators_keep_paths_as_arguments_and_use_the_discovered_compiler() {
    for generator in ["NMake Makefiles", "Ninja", "Ninja Multi-Config"] {
        let command = windows::configure_command(
            Path::new("custom cmake/cmake.exe"),
            Path::new("source & %name%"),
            Path::new("build dir"),
            Path::new("custom VS/cl.exe"),
            generator,
            "x86_64",
        )
        .unwrap();
        assert_eq!(command.get_program(), OsStr::new("custom cmake/cmake.exe"));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-S", "source & %name%"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-G", generator]));
        assert!(args.contains(&"-DCMAKE_CXX_COMPILER=custom VS/cl.exe"));
        assert!(args.contains(&"-DCMAKE_BUILD_TYPE=Release"));
        assert!(args.contains(&"--fresh"));
        assert!(!args.contains(&"-A"));
    }
}

#[test]
fn visual_studio_generator_uses_cargo_target_architecture() {
    for (arch, platform) in [("x86_64", "x64"), ("x86", "Win32"), ("aarch64", "ARM64")] {
        let command = windows::configure_command(
            Path::new("cmake.exe"),
            Path::new("source"),
            Path::new("build"),
            Path::new("cl.exe"),
            "Visual Studio 18 2026",
            arch,
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["-A", platform]));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("-DCMAKE_CXX_COMPILER="))
        );
    }
}

#[test]
fn empty_generator_and_unknown_visual_studio_architecture_are_rejected() {
    for (generator, arch) in [(" ", "x86_64"), ("Visual Studio 18 2026", "unknown")] {
        assert!(
            windows::configure_command(
                Path::new("cmake.exe"),
                Path::new("source"),
                Path::new("build"),
                Path::new("cl.exe"),
                generator,
                arch,
            )
            .is_err()
        );
    }
}
