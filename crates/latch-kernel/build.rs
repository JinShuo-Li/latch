#[cfg(windows)]
#[path = "build_support/windows.rs"]
mod windows;

#[cfg(windows)]
fn main() {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    for name in [
        "CMAKE",
        "CMAKE_GENERATOR",
        "PATH",
        "INCLUDE",
        "LIB",
        "LIBPATH",
        "VSINSTALLDIR",
        "VCINSTALLDIR",
        "VCToolsInstallDir",
        "VCToolsVersion",
        "VSCMD_ARG_TGT_ARCH",
        "WindowsSdkDir",
        "WindowsSDKVersion",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest dir"));
    let source = manifest.join("../../native/windows");
    let build = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("native");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/windows.rs");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture");
    // Reuse cc-rs discovery: a developer environment, then registered VS installs.
    let compiler = find_msvc_tools::find_tool(&arch, "cl.exe").unwrap_or_else(|| {
        panic!("Windows native backend requires MSVC C++ Build Tools and a Windows SDK for {arch}. Install the Desktop development with C++ workload, or build from a Developer PowerShell/Command Prompt configured for this target. No specific Visual Studio edition or installation directory is required.")
    });
    let environment: Vec<_> = compiler.env().into_iter().cloned().collect();
    let path = environment
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())
        .or_else(|| env::var_os("PATH"));
    let cmake = windows::find_cmake(
        env::var_os("CMAKE").as_deref(),
        path.as_deref(),
        compiler.path(),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let ninja = windows::find_executable(Path::new("ninja.exe"), path.as_deref());
    let generator = env::var("CMAKE_GENERATOR").unwrap_or_else(|_| {
        if ninja.is_some() {
            "Ninja"
        } else {
            "NMake Makefiles"
        }
        .to_owned()
    });
    let mut configure =
        windows::configure_command(&cmake, &source, &build, compiler.path(), &generator, &arch)
            .unwrap_or_else(|error| panic!("{error}"));
    configure.envs(environment.iter().cloned());
    if (generator == "Ninja" || generator == "Ninja Multi-Config")
        && let Some(ninja) = ninja
    {
        let mut argument = std::ffi::OsString::from("-DCMAKE_MAKE_PROGRAM=");
        argument.push(ninja);
        configure.arg(argument);
    }
    run(&mut configure, "configure Windows native backend");
    run(
        Command::new(&cmake)
            .envs(environment)
            .arg("--build")
            .arg(&build)
            .args(["--config", "Release"]),
        "build Windows native backend",
    );
    for filename in [
        "latch-windows-runner.exe",
        "msys-token-guard.exe",
        "msys-token-guard-hook.dll",
    ] {
        assert!(
            build.join("bin").join(filename).is_file(),
            "missing native artifact: {filename}"
        );
    }
    println!(
        "cargo:rustc-env=LATCH_WINDOWS_NATIVE_DIR={}",
        build.join("bin").display()
    );
}

#[cfg(windows)]
fn run(command: &mut std::process::Command, action: &str) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{action}: {error}"));
    assert!(
        output.status.success(),
        "{action} failed ({:?}): {}{}",
        command,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(not(windows))]
fn main() {}
