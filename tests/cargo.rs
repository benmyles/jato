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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jato"));
        cmd.current_dir(&self.root)
            .env("CARGO_HOME", self.temp.path().join("cargo"))
            .env("CARGO_TARGET_DIR", self.root.join("target"))
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
    for v in *b"AB" {
        fs::write(&blob, [v]).unwrap();
        p.clean();
        assert_eq!(p.run(&["run", "-q"]).stdout, format!("{v}\n").as_bytes());
    }
}
