//! Builds the small library that watched programs load, so that it can be
//! kept inside the `jato` executable. It is its own compilation because it
//! has to do without the standard library.
use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

fn main() {
    for input in [
        "build.rs",
        "src/fast/shim.rs",
        "src/fast/abi.rs",
        "src/fast/script_shim.rs",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let out = out_dir.join("libjato_fast.so");
    let script_out = out_dir.join("libjato_script.so");
    let target = env::var("TARGET").unwrap_or_default();
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    // The read-note library is built for Linux x86_64 and aarch64.
    let is_linux = target.contains("linux");
    let wanted = (target.starts_with("x86_64") || target.starts_with("aarch64")) && is_linux;
    let built = wanted
        && Command::new(&rustc)
            .args([
                "--edition=2024",
                "--crate-type=cdylib",
                "--crate-name=jato_fast",
            ])
            .args([
                "-C",
                "panic=abort",
                "-C",
                "opt-level=2",
                "-C",
                "codegen-units=1",
            ])
            .args([
                "-C",
                "debuginfo=0",
                "-C",
                "strip=symbols",
                "--cap-lints=allow",
            ])
            .arg("--target")
            .arg(&target)
            .arg("-o")
            .arg(&out)
            .arg("src/fast/shim.rs")
            .status()
            .is_ok_and(|status| status.success());
    if !built {
        if wanted {
            println!(
                "cargo:warning=the library for watched programs could not be built; every file call will be reported by the kernel"
            );
        }
        fs::write(&out, b"").expect("writing into OUT_DIR");
    }
    let script_built = wanted
        && Command::new(&rustc)
            .args([
                "--edition=2024",
                "--crate-type=cdylib",
                "--crate-name=jato_script",
            ])
            .args([
                "-C",
                "panic=abort",
                "-C",
                "opt-level=2",
                "-C",
                "codegen-units=1",
            ])
            .args([
                "-C",
                "debuginfo=0",
                "-C",
                "strip=symbols",
                "--cap-lints=allow",
            ])
            .arg("--target")
            .arg(&target)
            .arg("-o")
            .arg(&script_out)
            .arg("src/fast/script_shim.rs")
            .status()
            .is_ok_and(|status| status.success());
    if !script_built {
        if wanted {
            println!(
                "cargo:warning=the build-script interception library could not be built; build scripts will run uncached"
            );
        }
        fs::write(&script_out, b"").expect("writing into OUT_DIR");
    }
}
