#[cfg(windows)]
fn main() {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest dir"));
    let source = manifest.join("../../native/windows");
    let build = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("native");
    println!("cargo:rerun-if-changed={}", source.display());

    let program_files = env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    let installation = ["BuildTools", "Community", "Professional", "Enterprise"]
        .into_iter()
        .map(|edition| {
            program_files
                .join("Microsoft Visual Studio/2022")
                .join(edition)
        })
        .find(|root| root.join("Common7/Tools/VsDevCmd.bat").is_file())
        .expect("Windows native backend requires Visual Studio 2022 C++ Build Tools");
    let vsdev = installation.join("Common7/Tools/VsDevCmd.bat");
    let bundled_cmake =
        installation.join("Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe");
    let cmake = env::var_os("CMAKE").map(PathBuf::from).unwrap_or_else(|| {
        if bundled_cmake.is_file() {
            bundled_cmake
        } else {
            PathBuf::from("cmake.exe")
        }
    });
    std::fs::create_dir_all(&build).expect("create Windows native build dir");
    let script = build.join("build-native.cmd");
    std::fs::write(&script, format!(
        "@echo off\r\ncall \"{}\" -arch=x64 >nul\r\nif errorlevel 1 exit /b %errorlevel%\r\n\"{}\" -S \"%LATCH_CMAKE_SOURCE%\" -B \"%LATCH_CMAKE_BUILD%\" -G \"NMake Makefiles\"\r\nif errorlevel 1 exit /b %errorlevel%\r\n\"{}\" --build \"%LATCH_CMAKE_BUILD%\" --config Release\r\nexit /b %errorlevel%\r\n",
        vsdev.display(), cmake.display(), cmake.display()
    )).expect("write Windows native build command");
    let output = Command::new("cmd.exe")
        .args(["/d", "/c"])
        .arg(&script)
        .env("LATCH_CMAKE_SOURCE", &source)
        .env("LATCH_CMAKE_BUILD", &build)
        .output()
        .expect("start Windows native backend build");
    if !output.status.success() {
        panic!(
            "Windows native backend build failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for filename in [
        "latch-windows-runner.exe",
        "msys-token-guard.exe",
        "msys-token-guard-hook.dll",
    ] {
        assert!(
            Path::new(&build).join("bin").join(filename).is_file(),
            "missing native artifact: {filename}"
        );
    }
    println!(
        "cargo:rustc-env=LATCH_WINDOWS_NATIVE_DIR={}",
        build.join("bin").display()
    );
}

#[cfg(not(windows))]
fn main() {}
