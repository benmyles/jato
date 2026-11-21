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
    check_watcher(false);
}

#[test]
#[cfg(target_os = "linux")]
fn projected_watcher_observes_physical_edits_and_ignores_output_aliases() {
    if worktree_available() {
        check_watcher(true);
    }
}

fn check_watcher(projected: bool) {
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
    let mut cmd = p.command();
    if projected {
        cmd.args(["worktree", "--root", "."]);
    }
    let mut child = cmd
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
        .env("JATO_LOG", "debug")
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
        .stderr(Stdio::from(
            fs::File::create(p.temp.path().join("watch.log")).unwrap(),
        ))
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
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        1,
        "{}",
        fs::read_to_string(p.temp.path().join("watch.log")).unwrap()
    );
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

#[test]
fn compiler_environment_and_features_invalidate_independently() {
    let p = Project::new();
    for (path, features) in [
        (
            p.root.join("Cargo.toml"),
            "[features]\nfast=['leaf/fast']\n",
        ),
        (p.root.join("leaf/Cargo.toml"), "[features]\nfast=[]\n"),
    ] {
        let manifest = fs::read_to_string(&path).unwrap();
        write(path, &format!("{manifest}\n{features}"));
    }
    write(
        p.root.join("leaf/src/lib.rs"),
        r#"pub fn value() -> String {
        format!("{}|{:?}", cfg!(feature="fast"), option_env!("LEAF_ENV"))
    }"#,
    );
    for (fast, value) in [
        (false, None),
        (false, Some("")),
        (false, Some("a\nb\\c")),
        (true, Some("a\nb\\c")),
        (false, None),
    ] {
        let mut cmd = p.command();
        cmd.args(["run", "-q"]);
        if fast {
            cmd.args(["--features", "fast"]);
        }
        if let Some(v) = value {
            cmd.env("LEAF_ENV", v);
        }
        assert_eq!(
            success(&mut cmd).stdout,
            format!("{fast}|{value:?}\n").as_bytes()
        );
    }
    assert!(p.hits() > 0);
}

#[test]
fn damaged_shared_chunks_trigger_a_real_recompile_and_heal() {
    let p = Project::new();
    p.run(&["run", "-q"]);
    let db = rusqlite::Connection::open(p.temp.path().join("cache/artifacts-v1.sqlite3")).unwrap();
    db.execute("UPDATE chunks SET data=zeroblob(length(data))", [])
        .unwrap();
    drop(db);
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert!(p.stats()["counters"]["corrupt"].as_u64().unwrap_or(0) > 0);
    let hits = p.hits();
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert!(p.hits() > hits);
}

#[test]
fn large_embedded_assets_restore_and_content_edits_invalidate() {
    use std::os::unix::fs::FileExt;
    let p = Project::new();
    let asset = p.root.join("leaf/src/data.bin");
    let file = fs::File::create(&asset).unwrap();
    let len = (9 << 20) + 1;
    file.set_len(len).unwrap();
    file.write_all_at(b"link", 65537).unwrap();
    file.write_all_at(&[255], len - 1).unwrap();
    drop(file);
    write(
        p.root.join("leaf/src/lib.rs"),
        r#"pub fn value() -> u8 {
        let i = std::env::args().count()-1;
        include_bytes!("data.bin")[i]
    }"#,
    );
    let run = || {
        success(
            p.command()
                .env("JATO_LOG", "debug")
                .args(["run", "--release"]),
        )
    };
    assert_eq!(run().stdout, b"0\n");
    p.clean();
    let restored = run();
    assert_eq!(restored.stdout, b"0\n");
    assert!(
        String::from_utf8_lossy(&restored.stderr).contains("jato [leaf] hit"),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    fs::OpenOptions::new()
        .write(true)
        .open(asset)
        .unwrap()
        .write_all_at(&[23], 0)
        .unwrap();
    p.clean();
    assert_eq!(run().stdout, b"23\n");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn make_archive(dir: &Path, value: i32) {
    write(
        dir.join("value.c"),
        &format!("int jato_test_value(void) {{return {value};}}"),
    );
    success(
        Command::new("cc")
            .current_dir(dir)
            .args(["-c", "value.c", "-o", "value.o"]),
    );
    success(
        Command::new("ar")
            .current_dir(dir)
            .args(["crs", "libjato_test_value.a", "value.o"]),
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[test]
fn native_executable_tracks_archive_contents_and_negative_search_paths() {
    let p = Project::new();
    write(
        p.root.join("src/main.rs"),
        r#"
        #[link(name="jato_test_value", kind="static", modifiers="-bundle")]
        unsafe extern "C" { fn jato_test_value() -> i32; }
        fn main() { println!("{}",unsafe { jato_test_value() }); }
    "#,
    );
    let early = p.temp.path().join("earlier");
    let late = p.temp.path().join("later");
    fs::create_dir(&early).unwrap();
    fs::create_dir(&late).unwrap();
    let lib_path = std::env::join_paths([&early, &late]).unwrap();
    let build = || {
        let mut cmd = p.command();
        let toolchain_bin = Path::new(env!("CARGO")).parent().unwrap().to_owned();
        let path = std::env::join_paths(
            [toolchain_bin, "/usr/bin".into(), "/bin".into()]
                .into_iter()
                .chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                )),
        )
        .unwrap();
        cmd.env("LIBRARY_PATH", &lib_path)
            .env("PATH", path)
            .env_remove("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER")
            .env("JATO_LOG", "debug")
            .env_remove("JATO_LINK_CACHE")
            .args(["build", "--release"]);
        cmd.output().unwrap()
    };
    let output = p.root.join("target/release/app");
    make_archive(&late, 1);
    let cold = build();
    assert!(
        cold.status.success(),
        "{}",
        String::from_utf8_lossy(&cold.stderr)
    );
    let plain = fs::read(&output).unwrap();
    assert_eq!(success(&mut Command::new(&output)).stdout, b"1\n");
    p.clean();
    let warm = build();
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert_eq!(fs::read(&output).unwrap(), plain);
    // Opt-in assertion for CI and local qualification where tracing is available.
    if std::env::var_os("JATO_TEST_NATIVE_HITS").is_some() {
        assert!(
            String::from_utf8_lossy(&warm.stderr).contains("jato [app] hit"),
            "{}",
            String::from_utf8_lossy(&warm.stderr)
        );
    }
    make_archive(&early, 2);
    p.clean();
    let changed = build();
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(success(&mut Command::new(&output)).stdout, b"2\n");
    make_archive(&early, 3);
    p.clean();
    assert!(build().status.success());
    assert_eq!(success(&mut Command::new(&output)).stdout, b"3\n");
    fs::remove_file(early.join("libjato_test_value.a")).unwrap();
    p.clean();
    let reverted = build();
    assert!(
        reverted.status.success(),
        "{}",
        String::from_utf8_lossy(&reverted.stderr)
    );
    assert_eq!(success(&mut Command::new(&output)).stdout, b"1\n");
    if std::env::var_os("JATO_TEST_NATIVE_HITS").is_some() {
        assert!(
            String::from_utf8_lossy(&reverted.stderr).contains("jato [app] hit"),
            "{}",
            String::from_utf8_lossy(&reverted.stderr)
        );
    }
    fs::remove_file(late.join("libjato_test_value.a")).unwrap();
    p.clean();
    assert!(!build().status.success());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[test]
fn bundled_native_inputs_are_checked_before_restoring_the_rust_library() {
    let p = Project::new();
    let first = p.temp.path().join("first");
    let second = p.temp.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    make_archive(&second, 1);
    let native = "unsafe extern \"C\" { fn jato_test_value() -> i32; } pub fn value() -> i32 { unsafe { jato_test_value() } }";
    let search = format!(
        "println!(\"cargo:rustc-link-search=native={}\"); println!(\"cargo:rustc-link-search=native={}\");",
        first.display(),
        second.display()
    );
    write(
        p.root.join("leaf/build.rs"),
        &format!(
            "fn main() {{ {search} println!(\"cargo:rustc-link-lib=static=jato_test_value\"); }}"
        ),
    );
    write(p.root.join("leaf/src/lib.rs"), native);
    let run = || {
        p.command()
            .env("JATO_LOG", "debug")
            .env_remove("JATO_LINK_CACHE")
            .args(["--system-linker", "run", "--release"])
            .output()
            .unwrap()
    };
    let verify = |value: i32, hit| {
        p.clean();
        let out = run();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, format!("{value}\n").as_bytes());
        if hit && std::env::var_os("JATO_TEST_NATIVE_HITS").is_some() {
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("jato [leaf] hit"),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    };
    verify(1, false);
    verify(1, true);
    make_archive(&first, 2);
    verify(2, false);
    make_archive(&first, 3);
    verify(3, false);
    fs::remove_file(first.join("libjato_test_value.a")).unwrap();
    fs::remove_file(first.join("value.c")).unwrap();
    fs::remove_file(first.join("value.o")).unwrap();
    verify(1, true);
    write(
        p.root.join("leaf/build.rs"),
        &format!("fn main() {{ {search} }}"),
    );
    write(
        p.root.join("leaf/src/lib.rs"),
        &format!("#[link(name=\"jato_test_value\",kind=\"static\")] {native}"),
    );
    verify(1, false);
    verify(1, true);
    fs::remove_file(second.join("libjato_test_value.a")).unwrap();
    p.clean();
    assert!(!run().status.success());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[test]
fn clean_incremental_native_unit_restores_and_live_edits_keep_rustc_state() {
    let p = Project::new();
    let source = p.root.join("src/main.rs");
    let original = fs::read_to_string(&source).unwrap();
    let run = || {
        success(
            p.command()
                .env("CARGO_INCREMENTAL", "1")
                .env("JATO_LOG", "debug")
                .env_remove("JATO_LINK_CACHE")
                .args(["--system-linker", "run"]),
        )
    };
    let cold = run();
    assert_eq!(cold.stdout, b"one:none\n");
    write(&source, "fn main() { println!(\"edited\"); }");
    let edit = run();
    assert_eq!(edit.stdout, b"edited\n");
    assert!(
        String::from_utf8_lossy(&edit.stderr).contains("jato [app] active_incremental"),
        "{}",
        String::from_utf8_lossy(&edit.stderr)
    );
    p.clean();
    write(&source, &original);
    let restored = run();
    assert_eq!(restored.stdout, cold.stdout);
    if std::env::var_os("JATO_TEST_NATIVE_HITS").is_some() {
        assert!(
            String::from_utf8_lossy(&restored.stderr).contains("jato [app] hit"),
            "cold {}\nrestored {}",
            String::from_utf8_lossy(&cold.stderr),
            String::from_utf8_lossy(&restored.stderr)
        );
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[test]
fn compiled_proc_macro_is_reusable_but_its_expansion_runs_again() {
    let p = Project::new();
    write(
        p.root.join("m/Cargo.toml"),
        "[package]\nname='read_macro'\nversion='0.1.0'\nedition='2021'\n[lib]\nproc-macro=true\n",
    );
    write(
        p.root.join("m/src/lib.rs"),
        r#"#[proc_macro]
        pub fn read(_:proc_macro::TokenStream)->proc_macro::TokenStream {
            let s=std::fs::read_to_string(std::env::var_os("JATO_TEST_MACRO_INPUT").unwrap()).unwrap();
            format!("{:?}",s).parse().unwrap()
        }"#,
    );
    write(
        p.root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nread_macro={path='m'}\n",
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main(){println!("{}",read_macro::read!());}"#,
    );
    let side = p.temp.path().join("untracked");
    for value in ["one", "two"] {
        write(&side, value);
        p.clean();
        let out = success(
            p.command()
                .env_remove("JATO_LINK_CACHE")
                .env("JATO_TEST_MACRO_INPUT", &side)
                .env("JATO_LOG", "debug")
                .args(["--system-linker", "run", "--release"]),
        );
        assert_eq!(out.stdout, format!("{value}\n").as_bytes());
        if value == "two" && std::env::var_os("JATO_TEST_NATIVE_HITS").is_some() {
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("jato [read_macro] hit"),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[test]
fn cargo_probe_cache_replays_exact_bytes_and_leaves_source_queries_alone() {
    use std::process::Stdio;
    let p = Project::new();
    let native = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .unwrap();
    let rustc = Path::new(String::from_utf8(native.stdout).unwrap().trim()).join("bin/rustc");
    let expected = success(Command::new(&rustc).arg("-vV"));
    for _ in 0..2 {
        let out = success(p.command().arg(&rustc).arg("-vV"));
        assert_eq!(out.stdout, expected.stdout);
        assert_eq!(out.stderr, expected.stderr);
    }
    assert_eq!(p.stats()["counters"]["probe_hit"], 1);
    for name in ["first", "second"] {
        let mut cmd = p.command();
        let mut child = cmd
            .arg(&rustc)
            .args(["-", "--print=crate-name"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        writeln!(child.stdin.take().unwrap(), "#![crate_name=\"{name}\"]").unwrap();
        assert_eq!(
            child.wait_with_output().unwrap().stdout,
            format!("{name}\n").as_bytes()
        );
    }
    p.run(&["gc", "0"]);
    assert_eq!(p.stats()["probe_entries"], 0);
}

#[cfg(target_os = "linux")]
fn worktree_available() -> bool {
    let available = Command::new("bwrap")
        .args(["--dev-bind", "/", "/", "--", "true"])
        .output()
        .is_ok_and(|out| out.status.success());
    if std::env::var_os("JATO_TEST_WORKTREE").is_some() {
        assert!(available, "qualification requires working bubblewrap");
    }
    available
}

#[test]
#[cfg(target_os = "linux")]
fn projected_checkouts_restore_and_resolve_changed_modules() {
    if !worktree_available() {
        return;
    }
    let a = Project::new();
    let b = Project::new();
    let cache = a.temp.path().join("cache");
    let main = r#"fn main() { println!("{} {} {:?}", env!("CARGO_MANIFEST_DIR"), leaf::value(), std::env::args().skip(1).collect::<Vec<_>>()); }"#;
    for p in [&a, &b] {
        write(p.root.join("src/main.rs"), main);
    }
    let command = |p: &Project| {
        let mut c = p.command();
        c.env("CARGO_HOME", a.temp.path().join("cargo"))
            .env("JATO_LOG", "debug")
            .env("JATO_CACHE_DIR", &cache)
            .args(["worktree", "--root", "."]);
        if cfg!(all(target_arch = "x86_64", target_env = "gnu")) {
            c.arg("--system-linker");
        }
        c
    };
    let run = |p: &Project| {
        success(command(p).args(["run", "--release", "--", "--target-dir", "arg with spaces"]))
    };
    let physical = a.run(&["run", "--release"]);
    assert!(String::from_utf8_lossy(&physical.stdout).contains(a.root.to_str().unwrap()));
    let first = run(&a);
    let restored = run(&b);
    assert_eq!(first.stdout, restored.stdout);
    let out = String::from_utf8(restored.stdout).unwrap();
    assert!(out.contains(cache.join("view-v1/workspace").to_str().unwrap()));
    assert!(out.contains(r#"one:none ["--target-dir", "arg with spaces"]"#));
    let log = String::from_utf8_lossy(&restored.stderr);
    assert!(log.contains("jato [leaf] hit"), "{log}");
    if std::env::var_os("JATO_TEST_NATIVE_HITS").is_some() {
        assert!(log.contains("jato [app] hit"), "{log}");
    }
    let target = |p| {
        let paths: Value =
            serde_json::from_slice(&success(command(p).arg("paths")).stdout).unwrap();
        PathBuf::from(paths["target_directory"].as_str().unwrap())
    };
    let bin_a = target(&a).join("release/app");
    let bin_b = target(&b).join("release/app");
    assert!(bin_a.is_file() && bin_b.is_file());
    assert_eq!(a.run(&["run", "--release"]).stdout, physical.stdout);
    assert_eq!(fs::read_dir(cache.join("view-v1")).unwrap().count(), 0);
    write(b.root.join("leaf/src/text file"), "two");
    assert!(
        String::from_utf8(run(&b).stdout)
            .unwrap()
            .contains("two:none")
    );
    assert_eq!(
        success(Command::new(&bin_a).args(["--target-dir", "arg with spaces"])).stdout,
        first.stdout
    );
    success(command(&b).args(["cargo", "--color", "never", "clean"]));
    assert!(!bin_b.exists());
    write(b.root.join("leaf/src/text file"), "one");
    write(b.root.join("leaf/src/value/mod.rs"), "pub fn value() {}");
    let fail = command(&b).args(["build", "--release"]).output().unwrap();
    assert!(!fail.status.success());
    assert!(String::from_utf8_lossy(&fail.stderr).contains("E0761"));
}

#[test]
#[cfg(target_os = "linux")]
fn projected_commands_have_private_mounts_and_forward_exit_status() {
    if !worktree_available() {
        return;
    }
    let a = Project::new();
    let b = Project::new();
    let mut children = Vec::new();
    for (p, label) in [(&a, "left"), (&b, "right")] {
        let mut cmd = p.command();
        cmd.env("JATO_CACHE_DIR",a.temp.path().join("cache"))
            .args(["worktree","--root",".","exec","/bin/sh","-ec",
                r#"echo "$1" > "$CARGO_TARGET_DIR/marker"; touch "$2"; i=0; while test ! -f "$3" && test "$i" -lt 500; do sleep .01; i=$((i+1)); done; test -f "$3"; test "$(cat "$CARGO_TARGET_DIR/marker")" = "$1"; exit 17"#,
                "sh",label])
            .arg(p.temp.path().join("ready"))
            .arg(if label=="left" {b.temp.path()}else{a.temp.path()}.join("ready"));
        children.push(cmd.spawn().unwrap());
    }
    for child in &mut children {
        assert_eq!(child.wait().unwrap().code(), Some(17));
    }
    for (p, label) in [(&a, "left"), (&b, "right")] {
        assert_eq!(
            fs::read_to_string({
                let paths: Value = serde_json::from_slice(
                    &success(
                        p.command()
                            .env("JATO_CACHE_DIR", a.temp.path().join("cache"))
                            .args(["worktree", "--root", ".", "paths"]),
                    )
                    .stdout,
                )
                .unwrap();
                Path::new(paths["target_directory"].as_str().unwrap()).join("marker")
            })
            .unwrap()
            .trim(),
            label
        );
    }
    let ready = a.temp.path().join("started");
    let mut child = a
        .command()
        .args([
            "worktree",
            "exec",
            "/bin/sh",
            "-ec",
            r#"trap 'exit 42' TERM; touch "$1"; while :; do sleep .05; done"#,
            "sh",
        ])
        .arg(&ready)
        .spawn()
        .unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while !ready.exists() {
        assert!(child.try_wait().unwrap().is_none() && std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < until,
            "worktree did not forward SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(42));
}

#[test]
#[cfg(target_os = "linux")]
fn projected_cargo_obeys_config_and_target_overrides() {
    if !worktree_available() {
        return;
    }
    let p = Project::new();
    let output = p.temp.path().join("physical target");
    let build = p.temp.path().join("physical intermediate");
    let config = p.root.join("custom.toml");
    write(
        &config,
        &format!(
            "[build]\ntarget-dir={:?}\nbuild-dir={:?}\n",
            p.temp.path().join("unused"),
            build
        ),
    );
    let mut cmd = p.command();
    let out = success(
        cmd.env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .current_dir(p.root.join("leaf"))
            .arg("worktree")
            .arg("--root")
            .arg(&p.root)
            .args(["run", "--release", "--config"])
            .arg(&config)
            .arg("--target-dir")
            .arg(&output)
            .arg("--manifest-path")
            .arg(p.root.join("Cargo.toml")),
    );
    assert_eq!(out.stdout, b"one:none\n");
    let paths: Value = serde_json::from_slice(
        &success(
            p.command()
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("CARGO_BUILD_BUILD_DIR")
                .args(["worktree", "--root", ".", "paths", "--config"])
                .arg(&config)
                .arg("--target-dir")
                .arg(&output),
        )
        .stdout,
    )
    .unwrap();
    let final_dir = Path::new(paths["target_directory"].as_str().unwrap());
    assert!(final_dir.starts_with(&output) && final_dir.join("release/app").is_file());
    assert!(!p.temp.path().join("unused/release/app").exists());
    let meta =
        success(
            p.command()
                .args(["cargo", "metadata", "--no-deps", "--format-version", "1"]),
        );
    if serde_json::from_slice::<Value>(&meta.stdout)
        .unwrap()
        .get("build_directory")
        .is_some()
    {
        let build_dir = Path::new(paths["build_directory"].as_str().unwrap());
        assert!(
            build_dir.starts_with(build)
                && (build_dir.join("release/deps").is_dir()
                    || build_dir.join("release/build").is_dir())
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn concurrent_worktrees_keep_generated_files_private_in_a_global_target() {
    if !worktree_available() {
        return;
    }
    let a = Project::new();
    let b = Project::new();
    let common = a.temp.path().join("global-target");
    let mut children = Vec::new();
    for (p, word) in [(&a, "alpha"), (&b, "bravo")] {
        write(
            p.root.join("build.rs"),
            r#"fn main(){
            println!("cargo:rerun-if-changed=input");
            let out=std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
            std::fs::write(out.join("generated"),std::fs::read("input").unwrap()).unwrap();
        }"#,
        );
        write(p.root.join("input"), word);
        write(
            p.root.join("src/main.rs"),
            r#"fn main(){ print!("{}",include_str!(concat!(env!("OUT_DIR"),"/generated"))); }"#,
        );
        let mut cmd = p.command();
        cmd.env("CARGO_TARGET_DIR", &common)
            .env("CARGO_BUILD_BUILD_DIR", &common)
            .env("JATO_CACHE_DIR", a.temp.path().join("cache"))
            .env("CARGO_HOME", a.temp.path().join("cargo"))
            .args(["worktree", "--root", ".", "run", "-q"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        children.push((cmd.spawn().unwrap(), word));
    }
    for (child, word) in children {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, word.as_bytes());
    }
    assert_eq!(
        fs::read_dir(common.join(".jato-worktree")).unwrap().count(),
        2
    );
}

#[test]
#[cfg(target_os = "linux")]
fn projected_foreground_command_can_handle_terminal_interrupts() {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    if !worktree_available() {
        return;
    }
    let p = Project::new();
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let mut terminal = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut cmd = p.command();
    cmd.args([
        "worktree",
        "exec",
        "/bin/sh",
        "-ec",
        "trap 'exit 42' INT; echo ready; while :; do sleep .1; done",
    ])
    .stdin(slave.try_clone().unwrap())
    .stdout(slave.try_clone().unwrap())
    .stderr(slave);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(cmd);
    let mut fd = libc::pollfd {
        fd: terminal.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert!(unsafe { libc::poll(&mut fd, 1, 8000) } > 0);
    let mut line = String::new();
    BufReader::new(&terminal).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    terminal.write_all(&[3]).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(42));
            break;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("foreground interrupt timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn target_only_views_reuse_a_common_dependency_and_keep_host_source_paths() {
    if !worktree_available() {
        return;
    }
    let a = Project::new();
    let b = Project::new();
    let shared = a.temp.path().join("shared-leaf");
    fs::rename(a.root.join("leaf"), &shared).unwrap();
    let cache = a.temp.path().join("cache");
    let target = a.temp.path().join("global");
    for (p, name) in [(&a, "first"), (&b, "second")] {
        write(
            p.root.join("Cargo.toml"),
            &format!(
                "[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nleaf={{path={shared:?}}}\n"
            ),
        );
        write(
            p.root.join("src/main.rs"),
            r#"fn main(){println!("{} {}",leaf::value(),env!("CARGO_MANIFEST_DIR"));}"#,
        );
        let result = success(
            p.command()
                .env("CARGO_TARGET_DIR", &target)
                .env("CARGO_BUILD_BUILD_DIR", &target)
                .env("CARGO_HOME", a.temp.path().join("cargo"))
                .env("JATO_CACHE_DIR", &cache)
                .env("JATO_LOG", "debug")
                .args(["worktree", "run", "--release"]),
        );
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().trim(),
            format!("one:none {}", p.root.display())
        );
        if name == "second" {
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("jato [leaf] hit"),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    assert_eq!(
        fs::read_dir(target.join(".jato-worktree")).unwrap().count(),
        2
    );
}
