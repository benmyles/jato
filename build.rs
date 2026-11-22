//! Builds the small library that watched programs load, so that it can be
//! kept inside the `jato` executable. It is its own compilation because it
//! has to do without the standard library.
use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

fn main() {
    for input in ["build.rs", "src/fast/shim.rs", "src/fast/abi.rs"] {
        println!("cargo:rerun-if-changed={input}");
    }
    let out =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("libjato_fast.so");
    let target = env::var("TARGET").unwrap_or_default();
    // The library is written for one processor and one kernel. Elsewhere
    // Jato watches the way it always has.
    let wanted = target.starts_with("x86_64") && target.contains("linux");
    let built = wanted
        && Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
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
}
