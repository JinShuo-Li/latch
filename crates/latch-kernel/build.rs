fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    windows::build();
}

#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn run(command: &mut Command) {
        let status = command.status().expect("start the native Windows compiler");
        assert!(
            status.success(),
            "native Windows helper build failed: {command:?}"
        );
    }

    pub fn build() {
        let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("../../native/windows");
        let boundary = root.join("boundary");
        let detours = root.join("msys-guard/vendor/detours");
        let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        println!("cargo:rerun-if-changed={}", boundary.display());
        println!("cargo:rerun-if-changed={}", detours.display());
        let compiler = cc::Build::new().cpp(true).get_compiler();
        assert!(
            compiler.is_like_msvc(),
            "Windows requires the MSVC Rust toolchain"
        );
        let vendor = ["detours", "modules", "disasm", "image", "creatwth"];
        for name in vendor {
            run(compiler
                .to_command()
                .current_dir(&out)
                .args(["/nologo", "/MD", "/EHsc", "/std:c++17", "/c"])
                .arg(detours.join(format!("{name}.cpp")))
                .arg(format!("/Fo{}", out.join(format!("{name}.obj")).display())));
        }
        let compile = |name: &str| {
            run(compiler
                .to_command()
                .current_dir(&out)
                .args([
                    "/nologo",
                    "/MD",
                    "/EHsc",
                    "/std:c++20",
                    "/W4",
                    "/WX",
                    "/permissive-",
                    "/utf-8",
                    "/GS",
                    "/guard:cf",
                    "/sdl",
                    "/c",
                ])
                .arg(format!("/I{}", detours.display()))
                .arg(boundary.join(format!("{name}.cpp")))
                .arg(format!("/Fo{}", out.join(format!("{name}.obj")).display())));
        };
        compile("probe");
        compile("compat");
        let link = |object: &str, output: &Path, dll: bool| {
            let mut command = compiler.to_command();
            command.current_dir(&out).arg("/nologo").arg("/MD");
            if dll {
                command.arg("/LD");
            }
            command.arg(out.join(format!("{object}.obj")));
            for name in vendor {
                command.arg(out.join(format!("{name}.obj")));
            }
            command.arg(format!("/Fe{}", output.display()));
            command.args([
                "/link",
                "/DYNAMICBASE",
                "/NXCOMPAT",
                "/guard:cf",
                "/CETCOMPAT",
                "advapi32.lib",
                "user32.lib",
                "userenv.lib",
                "rpcrt4.lib",
                "ole32.lib",
            ]);
            if dll {
                command.arg("/EXPORT:DetourFinishHelperProcess,@1,NONAME");
            }
            run(&mut command);
        };
        link("compat", &out.join("latch-boundary-compat.dll"), true);
        link("probe", &out.join("latch-windows-runner.exe"), false);
    }
}
