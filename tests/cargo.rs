use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
}

fn write(path: impl AsRef<Path>, data: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project with spaces");
        write(
            root.join("Cargo.toml"),
            "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nleaf={path='leaf'}\n",
        );
        write(
            root.join("src/main.rs"),
            "fn main() { println!(\"{}\", leaf::value()); }",
        );
        write(
            root.join("leaf/Cargo.toml"),
            "[package]\nname='leaf'\nversion='0.1.0'\nedition='2021'\n",
        );
        write(
            root.join("leaf/src/lib.rs"),
            "mod value; pub use value::value;",
        );
        write(
            root.join("leaf/src/value.rs"),
            "pub fn value() -> String { format!(\"{}:{}\", include_str!(\"text file\"), option_env!(\"LEAF_ENV\").unwrap_or(\"none\")) }",
        );
        write(root.join("leaf/src/text file"), "one");
        Self { root, temp }
    }

    fn command(&self) -> Command {
        self.command_for(env!("CARGO_BIN_EXE_jato"))
    }

    fn command_for(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.root)
            .env("CARGO_HOME", self.temp.path().join("cargo"))
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_BUILD_BUILD_DIR", self.root.join("target"))
            .env_remove("JATO_DISABLE")
            .env("JATO_CACHE_DIR", self.temp.path().join("cache"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO", env!("CARGO"))
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env_remove("JATO_MAX_SIZE")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTC_BOOTSTRAP")
            .env_remove("LEAF_ENV");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        success(self.command().args(args))
    }
    fn clean(&self) {
        self.run(&["cargo", "clean"]);
    }
    fn stats(&self) -> Value {
        serde_json::from_slice(&self.run(&["stats", "--json"]).stdout).unwrap()
    }
    fn hits(&self) -> u64 {
        self.stats()["counters"]["hit"].as_u64().unwrap_or(0)
    }
}

fn success(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn rebuilds_variants_and_invalidates_includes_env_and_resolution() {
    let p = Project::new();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert!(p.hits() > 0);
    write(p.root.join("leaf/src/text file"), "two");
    assert_eq!(
        success(p.command().env("LEAF_ENV", "set").args(["run", "-q"])).stdout,
        b"two:set\n"
    );
    let hits = p.hits();
    write(p.root.join("leaf/src/text file"), "one");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert!(p.hits() > hits);
    // A new competing module can invalidate a compile even if every file in
    // the previous dep-info has identical contents.
    write(p.root.join("leaf/src/value/mod.rs"), "pub fn value() {}");
    p.clean();
    let fail = p.command().arg("build").output().unwrap();
    assert!(!fail.status.success());
    assert!(String::from_utf8_lossy(&fail.stderr).contains("E0761"));
}

#[test]
fn metadata_restore_replays_diagnostics_and_cache_failure_preserves_cargo() {
    let p = Project::new();
    write(
        p.root.join("src/main.rs"),
        "fn main() { let unused_marker = leaf::value(); }",
    );
    let args = ["check", "--all-targets", "--message-format=json"];
    p.run(&args);
    p.clean();
    let out = p.run(&args);
    let messages: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(messages.iter().any(|m| {
        m["reason"] == "compiler-message"
            && m["message"]["message"]
                .as_str()
                .is_some_and(|s| s.contains("unused_marker"))
    }));
    assert!(p.hits() >= 2, "{}", p.stats());
    p.clean();
    let file = p.temp.path().join("ordinary-file");
    write(&file, "unusable cache");
    success(p.command().env("JATO_CACHE_DIR", &file).args(["check"]));
    write(
        p.root.join("src/main.rs"),
        "fn main() { missing_function(); }",
    );
    let out = p
        .command()
        .env("JATO_CACHE_DIR", &file)
        .arg("check")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(101));
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing_function"));
}

#[test]
fn forwarded_test_and_program_arguments_keep_their_boundaries() {
    let p = Project::new();
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }",
    );
    assert_eq!(
        p.run(&["run", "-q", "--", "two words", "--flag"]).stdout,
        b"[\"two words\", \"--flag\"]\n"
    );
    let out = p
        .command()
        .args([OsStr::new("test"), OsStr::new("--"), OsStr::new("--list")])
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn watcher_checks_edited_package_then_workspace_and_ignores_build_outputs() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let p = Project::new();
    let manifest = p.root.join("Cargo.toml");
    let mut contents = fs::read_to_string(&manifest).unwrap();
    contents.push_str("\n[workspace]\nmembers=['leaf']\n");
    write(manifest, &contents);
    p.run(&["check", "--workspace"]); // establish lockfile and initial target
    let mut child = p
        .command()
        .args(["watch", "--debounce", "40"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let (send, recv) = mpsc::channel();
    std::thread::spawn(move || {
        for line in lines.map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Guard(child);
    let wait = |marker: &str| {
        let until = Instant::now() + Duration::from_secs(15);
        let mut seen = String::new();
        loop {
            let line = recv
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("expected {marker:?}: {e}\n{seen}"));
            if line.contains(marker) {
                break;
            }
            seen.push_str(&line);
            seen.push('\n');
        }
    };
    wait("finished in");
    write(p.root.join("target/ignored"), "build output");
    assert!(recv.recv_timeout(Duration::from_millis(180)).is_err());
    write(p.root.join("leaf/src/text file"), "edited");
    wait("cargo check -p leaf");
    wait("cargo check --workspace");
    wait("finished in");
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGINT);
    }
    assert_eq!(child.0.wait().unwrap().code(), Some(130));
}

#[test]
fn generated_sources_and_proc_macro_reexports_never_replay_stale_values() {
    let p = Project::new();
    write(
        p.root.join("leaf/build.rs"),
        r#"fn main() {
        println!("cargo:rerun-if-env-changed=GENERATED_VALUE");
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        let value = std::env::var("GENERATED_VALUE").unwrap_or_default();
        std::fs::write(out.join("generated.rs"), format!("pub fn value()-> &'static str {{ {:?} }}", value)).unwrap();
    }"#,
    );
    write(
        p.root.join("leaf/src/lib.rs"),
        r#"include!(concat!(env!("OUT_DIR"), "/generated.rs"));"#,
    );
    for v in ["first", "second", "first"] {
        assert_eq!(
            success(p.command().env("GENERATED_VALUE", v).args(["run", "-q"])).stdout,
            format!("{v}\n").as_bytes()
        );
    }

    write(
        p.root.join("mac/Cargo.toml"),
        "[package]\nname='mac'\nversion='0.1.0'\nedition='2021'\n[lib]\nproc-macro=true\n",
    );
    write(
        p.root.join("mac/src/lib.rs"),
        r#"extern crate proc_macro;
        #[proc_macro] pub fn external(_:proc_macro::TokenStream)->proc_macro::TokenStream {
            let value = std::fs::read_to_string(std::env::var_os("JATO_MACRO_FILE").unwrap()).unwrap();
            format!("{:?}", value).parse().unwrap()
        }"#,
    );
    let manifest = p.root.join("leaf/Cargo.toml");
    write(
        &manifest,
        &(fs::read_to_string(&manifest).unwrap() + "\n[dependencies]\nmac={path='../mac'}\n"),
    );
    write(p.root.join("leaf/src/lib.rs"), "pub use mac::external;");
    write(
        p.root.join("src/lib.rs"),
        "pub fn value() -> &'static str { leaf::external!() }",
    );
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{}\", app::value()); }",
    );
    let hidden = p.temp.path().join("macro-input");
    for v in ["first", "changed"] {
        write(&hidden, v);
        p.clean();
        assert_eq!(
            success(
                p.command()
                    .env("JATO_MACRO_FILE", &hidden)
                    .args(["run", "-q"])
            )
            .stdout,
            format!("{v}\n").as_bytes()
        );
    }
}

#[test]
fn two_cargo_targets_can_publish_to_one_cache_while_it_is_pruned() {
    let p = Project::new();
    p.run(&["cargo", "generate-lockfile"]);
    let mut a = p.command().args(["check", "-q"]).spawn().unwrap();
    let mut b = p
        .command()
        .env("CARGO_TARGET_DIR", p.temp.path().join("second target"))
        .env("CARGO_BUILD_BUILD_DIR", p.temp.path().join("second target"))
        .args(["check", "-q"])
        .spawn()
        .unwrap();
    for _ in 0..4 {
        p.run(&["gc", "1MiB"]);
    }
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    p.clean();
    p.run(&["check"]);
    assert!(p.hits() > 0);
    p.run(&["gc", "0"]);
    let stats = p.stats();
    assert_eq!(stats["entries"], 0);
    assert_eq!(stats["charged_bytes"], 0);
    assert_eq!(stats["chunks"], 0);
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn assembly_includes_are_left_to_the_compiler() {
    let p = Project::new();
    let blob = p.temp.path().join("assembler-input");
    write(
        p.root.join("leaf/src/lib.rs"),
        &format!(
            r##"
        core::arch::global_asm!(r#".section .rodata
            .global jato_byte
            jato_byte: .incbin "{}""#);
        extern "C" {{ static jato_byte: u8; }}
        pub fn value() -> u8 {{ unsafe {{ jato_byte }} }}
    "##,
            blob.display()
        ),
    );
    // Even with the assembler file absent, metadata emission can succeed.
    // The resulting check record must never authorize a codegen cache hit.
    p.run(&["check"]);
    p.clean();
    p.run(&["check"]);
    assert!(p.hits() > 0);
    let fail = p.command().arg("build").output().unwrap();
    assert!(!fail.status.success());
    for v in *b"AB" {
        fs::write(&blob, [v]).unwrap();
        p.clean();
        assert_eq!(p.run(&["run", "-q"]).stdout, format!("{v}\n").as_bytes());
    }
}

#[test]
fn standard_wrapper_restores_the_exact_artifacts_and_side_outputs_bypass() {
    fn collect(path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(path).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                collect(&e.path(), files);
            } else if e.file_name().to_string_lossy().contains("leaf")
                && matches!(
                    e.path().extension().and_then(OsStr::to_str),
                    Some("rlib" | "rmeta" | "d")
                )
            {
                files.insert(e.path(), fs::read(e.path()).unwrap());
            }
        }
    }
    let p = Project::new();
    let build = || {
        success(
            p.command_for(env!("CARGO"))
                .env("RUSTC_WRAPPER", env!("CARGO_BIN_EXE_jato"))
                .args(["build", "-q"]),
        );
        let mut files = Default::default();
        collect(&p.root.join("target"), &mut files);
        files
    };
    let cold = build();
    assert!(
        cold.keys()
            .any(|p| p.extension() == Some(OsStr::new("rlib")))
    );
    p.clean();
    assert_eq!(cold, build());
    assert!(p.hits() > 0);
    let hits = p.hits();
    p.clean();
    success(p.command().env("RUSTFLAGS", "--emit=llvm-ir").arg("build"));
    assert_eq!(p.hits(), hits);
}

#[test]
fn watcher_restarts_a_running_program_and_preserves_custom_arguments() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let p = Project::new();
    let log = p.temp.path().join("starts");
    let source = p.root.join("src/main.rs");
    let main = r#"use std::{env, fs::OpenOptions, io::Write, time::Duration};
        fn main() {
            let mut log = OpenOptions::new().append(true).create(true).open(env::var_os("WATCH_STARTS").unwrap()).unwrap();
            writeln!(log, "{} {}", std::process::id(), env::args().nth(1).unwrap()).unwrap();
            std::thread::sleep(Duration::from_secs(12));
        }"#;
    write(&source, main);
    p.run(&["build", "-q"]);
    let mut child = p
        .command()
        .env("WATCH_STARTS", &log)
        .current_dir(p.temp.path())
        .args([
            "watch",
            "--debounce",
            "25",
            "--",
            "run",
            "-q",
            "--manifest-path",
        ])
        .arg(p.root.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(p.root.join("custom output"))
        .args(["--", "two words"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct StopOnDrop(u32);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe {
                    libc::kill(self.0 as i32, libc::SIGTERM);
                }
            }
        }
    }
    let mut guard = StopOnDrop(child.id());
    let wait_for_runs = |count| {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let runs = fs::read_to_string(&log).unwrap_or_default();
            if runs.lines().count() >= count {
                return runs;
            }
            assert!(Instant::now() < end, "watch did not start run {count}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for_runs(1);
    std::thread::sleep(Duration::from_millis(180));
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    write(&source, &format!("{main}\n// save"));
    let runs = wait_for_runs(2);
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    assert_eq!(child.wait().unwrap().code(), Some(130));
    guard.0 = 0;
    for run in runs.lines() {
        let (pid, arg) = run.split_once(' ').unwrap();
        assert_eq!(arg, "two words");
        #[cfg(target_os = "linux")]
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            assert!(stat.contains(") Z "), "program still running: {stat}");
        }
    }
}

#[test]
fn nested_workspace_wrapper_runs_with_original_compiler_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new();
    let helper = p.temp.path().join("my-driver");
    let log = p.temp.path().join("driver-ran");
    write(
        &helper,
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$DRIVER_LOG\"\nexec \"$@\"\n",
    );
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![p.temp.path().to_owned()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    success(
        p.command()
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("RUSTC_WORKSPACE_WRAPPER", "my-driver")
            .env("DRIVER_LOG", &log)
            .arg("check"),
    );
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .lines()
            .any(|p| Path::new(p).file_name() == Some(OsStr::new("rustc")))
    );
}
