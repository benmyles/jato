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
        write(root.join("Cargo.toml"), "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nleaf={path='leaf'}\n");
        write(root.join("src/main.rs"), "fn main() { println!(\"{}\", leaf::value()); }");
        write(root.join("leaf/Cargo.toml"), "[package]\nname='leaf'\nversion='0.1.0'\nedition='2021'\n");
        write(root.join("leaf/src/lib.rs"), "mod value; pub use value::value;");
        write(root.join("leaf/src/value.rs"), "pub fn value() -> String { format!(\"{}:{}\", include_str!(\"text file\"), option_env!(\"LEAF_ENV\").unwrap_or(\"none\")) }");
        write(root.join("leaf/src/text file"), "one");
        Self { root, temp }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jato"));
        cmd.current_dir(&self.root)
            .env("CARGO_HOME",self.temp.path().join("cargo"))
            .env("CARGO_TARGET_DIR",self.root.join("target"))
            .env("JATO_CACHE_DIR",self.temp.path().join("cache"))
            .env("CARGO_NET_OFFLINE","true").env("CARGO",env!("CARGO"))
            .env("RUSTC_WORKSPACE_WRAPPER","").env_remove("JATO_MAX_SIZE")
            .env_remove("RUSTFLAGS").env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTC_BOOTSTRAP").env_remove("LEAF_ENV");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output { success(self.command().args(args)) }
    fn clean(&self) { self.run(&["cargo", "clean"]); }
    fn stats(&self) -> Value { serde_json::from_slice(&self.run(&["stats","--json"]).stdout).unwrap() }
    fn hits(&self) -> u64 { self.stats()["counters"]["hit"].as_u64().unwrap_or(0) }
}

fn success(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}\n{}", String::from_utf8_lossy(&out.stderr));
    out
}

#[test]
fn rebuilds_variants_and_invalidates_includes_env_and_resolution() {
    let p = Project::new();
    assert_eq!(p.run(&["run","-q"]).stdout,b"one:none\n");
    p.clean();
    assert_eq!(p.run(&["run","-q"]).stdout,b"one:none\n");
    assert!(p.hits() > 0);
    write(p.root.join("leaf/src/text file"),"two");
    assert_eq!(success(p.command().env("LEAF_ENV","set").args(["run","-q"])).stdout,b"two:set\n");
    let hits = p.hits();
    write(p.root.join("leaf/src/text file"),"one");
    assert_eq!(p.run(&["run","-q"]).stdout,b"one:none\n");
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
    write(p.root.join("src/main.rs"),"fn main() { let unused_marker = leaf::value(); }");
    let args = ["check","--all-targets","--message-format=json"];
    p.run(&args);
    p.clean();
    let out = p.run(&args);
    let messages: Vec<Value> = String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(messages.iter().any(|m| m["reason"]=="compiler-message" && m["message"]["message"].as_str().is_some_and(|s| s.contains("unused_marker"))));
    assert!(p.hits() >= 2, "{}", p.stats());
    p.clean();
    let file = p.temp.path().join("ordinary-file");
    write(&file, "unusable cache");
    success(p.command().env("JATO_CACHE_DIR", &file).args(["check"]));
    write(p.root.join("src/main.rs"),"fn main() { missing_function(); }");
    let out = p.command().env("JATO_CACHE_DIR", &file).arg("check").output().unwrap();
    assert_eq!(out.status.code(),Some(101));
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing_function"));
}

#[test]
fn forwarded_test_and_program_arguments_keep_their_boundaries() {
    let p = Project::new();
    write(p.root.join("src/main.rs"),"fn main() { println!(\"{:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }");
    assert_eq!(p.run(&["run","-q","--","two words","--flag"]).stdout,b"[\"two words\", \"--flag\"]\n");
    let out = p.command().args([OsStr::new("test"),OsStr::new("--"),OsStr::new("--list")]).output().unwrap();
    assert!(out.status.success());
}
