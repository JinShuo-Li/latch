use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn find_executable(program: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    let candidates = if program.components().count() > 1 || program.is_absolute() {
        vec![program.to_owned()]
    } else {
        path.map(std::env::split_paths)
            .into_iter()
            .flatten()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(program))
            .collect()
    };
    candidates.into_iter().find_map(|candidate| {
        if candidate.is_file() {
            Some(candidate)
        } else if candidate.extension().is_none() {
            let executable = candidate.with_extension("exe");
            executable.is_file().then_some(executable)
        } else {
            None
        }
    })
}

pub fn find_cmake(
    explicit: Option<&OsStr>,
    path: Option<&OsStr>,
    compiler: &Path,
) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit {
        return find_executable(Path::new(explicit), path).ok_or_else(|| {
            format!("CMAKE={explicit:?} does not name an existing executable. Set CMAKE to the cmake.exe path (without surrounding quotes), or put CMake on PATH.")
        });
    }
    find_executable(Path::new("cmake.exe"), path)
        .or_else(|| compiler.ancestors().find_map(|root| {
            let bundled = root.join("Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe");
            bundled.is_file().then_some(bundled)
        }))
        .ok_or_else(|| "Windows native backend requires CMake 3.25 or newer. Install CMake and add it to PATH, set CMAKE to its executable, or install the C++ CMake tools component in Visual Studio Build Tools.".to_owned())
}

pub fn configure_command(
    cmake: &Path,
    source: &Path,
    build: &Path,
    compiler: &Path,
    generator: &str,
    arch: &str,
) -> Result<Command, String> {
    if generator.trim().is_empty() {
        return Err("CMAKE_GENERATOR must not be empty".to_owned());
    }
    let mut command = Command::new(cmake);
    // Reset stale CMake configuration when the selected compiler/generator changes.
    command
        .arg("--fresh")
        .arg("-S")
        .arg(source)
        .arg("-B")
        .arg(build)
        .arg("-G")
        .arg(generator)
        .arg("-DCMAKE_BUILD_TYPE=Release");
    if generator.starts_with("Visual Studio ") {
        let platform = match arch {
            "x86_64" => "x64",
            "x86" => "Win32",
            "aarch64" => "ARM64",
            _ => {
                return Err(format!(
                    "unsupported Visual Studio target architecture: {arch}"
                ));
            }
        };
        command.arg("-A").arg(platform);
    } else {
        let mut argument = OsString::from("-DCMAKE_CXX_COMPILER=");
        argument.push(compiler);
        command.arg(argument);
    }
    Ok(command)
}
