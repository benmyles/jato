//! Jato exercised through real Cargo projects.
//!
//! Each project has its own Cargo home, target directory and cache. Set
//! JATO_TEST_DIR to hold them on a particular filesystem: on Btrfs or XFS
//! results are shared by cloning, elsewhere they are stored compressed.
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

fn scratch() -> tempfile::TempDir {
    let base = std::env::var_os("JATO_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("jato-test-")
        .tempdir_in(base)
        .unwrap()
}

/// An application with one library that includes a text file and reads an
/// optional compile-time environment variable.
fn populate(root: &Path) {
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
}

impl Project {
    fn new() -> Self {
        Self::named("project")
    }

    fn named(directory: &str) -> Self {
        let temp = scratch();
        let root = temp.path().join(directory);
        populate(&root);
        Self { root, temp }
    }

    fn command(&self) -> Command {
        self.command_in(&self.root)
    }

    /// Jato run in `root` with this project's cache and Cargo home.
    fn command_in(&self, root: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jato"));
        cmd.current_dir(root)
            .env("CARGO_HOME", self.temp.path().join("cargo"))
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("JATO_CACHE_DIR", self.temp.path().join("cache"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO", env!("CARGO"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env_remove("JATO_DISABLE")
            .env_remove("JATO_LOG")
            .env_remove("JATO_MAX_SIZE")
            .env_remove("JATO_KEY_ENV")
            .env_remove("JATO_OBSERVED")
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
    fn count(&self, name: &str) -> u64 {
        self.stats()["counters"][name].as_u64().unwrap_or(0)
    }
    fn hits(&self) -> u64 {
        self.count("hit")
    }
    fn misses(&self) -> u64 {
        self.count("miss")
    }
    /// Counters over one run of `work`.
    fn during(&self, work: impl FnOnce()) -> (u64, u64) {
        let (hits, misses) = (self.hits(), self.misses());
        work();
        (self.hits() - hits, self.misses() - misses)
    }
}

fn success(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// The option that makes rustc leave linking to the C compiler's own
/// linker, on compilers that would otherwise bring theirs.
fn system_linker() -> &'static str {
    let rustc = Path::new(env!("CARGO")).with_file_name("rustc");
    let accepted = Command::new(rustc)
        .args(["-Clinker-features=-lld", "--print", "sysroot"])
        .output()
        .is_ok_and(|out| out.status.success());
    if accepted {
        "-Clinker-features=-lld"
    } else {
        ""
    }
}

/// Whether this system lets Jato watch what a compiler reads. Without
/// that nothing is stored, and only the pass-through behaviour applies.
fn observes() -> bool {
    cfg!(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
}

/// The decisions Jato logged for the crate `name`, in order.
fn decisions(output: &Output, name: &str) -> Vec<String> {
    let prefix = format!("jato [{name}] ");
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_cleared_target_is_restored_and_edits_are_compiled() {
    let p = Project::new();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    if !observes() {
        return;
    }
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    assert_eq!(misses, 0, "{}", p.stats());
    assert!(hits >= 2, "{}", p.stats());

    // An included file and a compile-time variable are both inputs.
    write(p.root.join("leaf/src/text file"), "two");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"two:none\n");
    assert_eq!(
        success(p.command().env("LEAF_ENV", "set").args(["run", "-q"])).stdout,
        b"two:set\n"
    );
    // A variable no crate reads is not.
    let (_, misses) = p.during(|| {
        p.clean();
        success(p.command().env("UNRELATED", "x").args(["build", "-q"]));
    });
    assert_eq!(misses, 0);

    // Undoing an edit returns to a result that is already known.
    write(p.root.join("leaf/src/text file"), "one");
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}

#[test]
fn a_new_file_the_compiler_would_find_is_not_overlooked() {
    let p = Project::new();
    p.run(&["build", "-q"]);
    // rustc looks for a module in two places. A second file makes the
    // module ambiguous while every file read before stays the same.
    write(p.root.join("leaf/src/value/mod.rs"), "pub fn value() {}");
    p.clean();
    let fail = p.command().arg("build").output().unwrap();
    assert!(!fail.status.success());
    assert!(String::from_utf8_lossy(&fail.stderr).contains("E0761"));
    fs::remove_dir_all(p.root.join("leaf/src/value")).unwrap();
    let (_, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    if observes() {
        assert_eq!(misses, 0);
    }
}

#[test]
fn diagnostics_are_replayed_and_failures_are_never_reused() {
    let p = Project::new();
    write(
        p.root.join("src/main.rs"),
        "fn main() { let unused_marker = leaf::value(); }",
    );
    let args = ["check", "--all-targets", "--message-format=json"];
    let warnings = |out: &Output| -> Vec<String> {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|m| m["reason"] == "compiler-message")
            .map(|m| m["message"]["rendered"].as_str().unwrap().to_owned())
            .collect()
    };
    let first = warnings(&p.run(&args));
    assert!(first.iter().any(|w| w.contains("unused_marker")));
    p.clean();
    let (hits, _) = p.during(|| assert_eq!(warnings(&p.run(&args)), first));
    if observes() {
        assert!(hits >= 2, "{}", p.stats());
    }

    // An unusable cache directory leaves Cargo working.
    p.clean();
    let file = p.temp.path().join("ordinary-file");
    write(&file, "unusable cache");
    success(p.command().env("JATO_CACHE_DIR", &file).args(["check"]));

    // A failed compilation reports the compiler's own error and status each
    // time, and the corrected source compiles.
    write(
        p.root.join("src/main.rs"),
        "fn main() { missing_function(); }",
    );
    for _ in 0..2 {
        let out = p.command().arg("check").output().unwrap();
        assert_eq!(out.status.code(), Some(101));
        assert!(String::from_utf8_lossy(&out.stderr).contains("missing_function"));
    }
    write(p.root.join("src/main.rs"), "fn main() {}");
    p.run(&["check"]);
}

#[test]
fn another_target_directory_and_another_checkout_share_results() {
    if !observes() {
        return;
    }
    let p = Project::new();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    let original = fs::read(p.root.join("target/debug/app")).unwrap();

    // The same checkout built into a directory Cargo has never used.
    let other_target = p.temp.path().join("elsewhere/out");
    let (hits, misses) = p.during(|| {
        let out = success(
            p.command()
                .env("CARGO_TARGET_DIR", &other_target)
                .args(["run", "-q"]),
        );
        assert_eq!(out.stdout, b"one:none\n");
    });
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    assert_eq!(fs::read(other_target.join("debug/app")).unwrap(), original);

    // The same sources checked out somewhere else.
    let copy = p.temp.path().join("second/checkout");
    populate(&copy);
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&copy).args(["run", "-q"]));
        assert_eq!(out.stdout, b"one:none\n");
    });
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    assert_eq!(fs::read(copy.join("target/debug/app")).unwrap(), original);

    // Each restored file is the build's own: changing one changes no other.
    fs::write(copy.join("target/debug/app"), "overwritten").unwrap();
    assert_eq!(fs::read(p.root.join("target/debug/app")).unwrap(), original);
    fs::remove_dir_all(&other_target).unwrap();
    success(
        p.command()
            .env("CARGO_TARGET_DIR", &other_target)
            .args(["build", "-q"]),
    );
    assert_eq!(fs::read(other_target.join("debug/app")).unwrap(), original);

    // An edit in one checkout is that checkout's alone.
    write(copy.join("leaf/src/text file"), "copy");
    assert_eq!(
        success(p.command_in(&copy).args(["run", "-q"])).stdout,
        b"copy:none\n"
    );
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
}

#[test]
fn output_that_names_its_checkout_is_not_used_in_another() {
    if !observes() {
        return;
    }
    let p = Project::new();
    // The library keeps the absolute path of its own directory.
    write(
        p.root.join("leaf/src/value.rs"),
        "pub fn value() -> String { env!(\"CARGO_MANIFEST_DIR\").to_owned() }",
    );
    let here = format!("{}\n", p.root.join("leaf").display());
    assert_eq!(p.run(&["run", "-q"]).stdout, here.as_bytes());
    let copy = p.temp.path().join("moved/checkout");
    populate(&copy);
    fs::copy(
        p.root.join("leaf/src/value.rs"),
        copy.join("leaf/src/value.rs"),
    )
    .unwrap();
    let there = format!("{}\n", copy.join("leaf").display());
    let out = success(
        p.command_in(&copy)
            .env("JATO_LOG", "debug")
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, there.as_bytes());
    assert!(
        decisions(&out, "leaf")
            .iter()
            .any(|d| d.contains("which is not where this build is")),
        "{:?}",
        decisions(&out, "leaf")
    );
    // In its own checkout it is still reused, in any target directory.
    let (hits, misses) = p.during(|| {
        let out = success(
            p.command()
                .env("CARGO_TARGET_DIR", p.temp.path().join("t2"))
                .args(["run", "-q"]),
        );
        assert_eq!(out.stdout, here.as_bytes());
    });
    assert_eq!((hits, misses), (2, 0));
}

#[test]
fn a_path_cargo_must_escape_keeps_its_results_where_they_were_made() {
    let p = Project::named("project with spaces");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    if observes() {
        assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    }
    write(p.root.join("leaf/src/text file"), "two");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"two:none\n");
}

/// A procedural macro crate whose macros read things the compiler is never
/// told about: a file, the names in a directory, and a variable.
fn add_macros(root: &Path) {
    write(
        root.join("macros/Cargo.toml"),
        "[package]\nname='macros'\nversion='0.1.0'\nedition='2021'\n[lib]\nproc-macro=true\n",
    );
    write(
        root.join("macros/src/lib.rs"),
        r#"use proc_macro::TokenStream;
fn literal(text: String) -> TokenStream { format!("{text:?}").parse().unwrap() }
fn dir() -> String { std::env::var("CARGO_MANIFEST_DIR").unwrap() }
#[proc_macro]
pub fn hidden(_: TokenStream) -> TokenStream {
    literal(std::fs::read_to_string(format!("{}/hidden.txt", dir())).unwrap())
}
#[proc_macro]
pub fn names(_: TokenStream) -> TokenStream {
    let mut names: Vec<String> = std::fs::read_dir(format!("{}/assets", dir())).unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    literal(names.join(","))
}
#[proc_macro]
pub fn setting(_: TokenStream) -> TokenStream {
    literal(std::env::var("MACRO_SETTING").unwrap_or_default())
}
"#,
    );
}

#[test]
fn what_a_proc_macro_reads_is_an_input_of_the_crate_using_it() {
    if !observes() {
        return;
    }
    let p = Project::new();
    add_macros(&p.root);
    // The application reaches the macros through a library that re-exports
    // them, so it has no direct dependency on the macro crate.
    write(
        p.root.join("leaf/Cargo.toml"),
        "[package]\nname='leaf'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmacros={path='../macros'}\n",
    );
    write(p.root.join("leaf/src/lib.rs"), "pub use macros::*;");
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{} {}\", leaf::hidden!(), leaf::names!()); }",
    );
    write(p.root.join("hidden.txt"), "first");
    write(p.root.join("assets/a"), "");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"first a\n");
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"first a\n"));
    assert_eq!((hits, misses), (3, 0), "{}", p.stats());

    // Cargo does not know about either input, so the crate has to be
    // touched for Cargo to run the compiler at all. Jato must then notice
    // what Cargo could not.
    let touch = || {
        let main = p.root.join("src/main.rs");
        let text = fs::read_to_string(&main).unwrap();
        write(&main, &format!("{text} "));
        write(&main, &text);
    };
    write(p.root.join("hidden.txt"), "second");
    touch();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"second a\n");
    write(p.root.join("assets/b"), "");
    touch();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"second a,b\n");
    // Back to a state seen before.
    fs::remove_file(p.root.join("assets/b")).unwrap();
    touch();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"second a\n"));
    assert_eq!((hits, misses), (1, 0), "{}", p.stats());
}

#[test]
fn a_variable_a_proc_macro_reads_can_be_declared() {
    if !observes() {
        return;
    }
    let p = Project::new();
    add_macros(&p.root);
    write(
        p.root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmacros={path='macros'}\n",
    );
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{}\", macros::setting!()); }",
    );
    let run = |value: &str| {
        let main = p.root.join("src/main.rs");
        let text = fs::read_to_string(&main).unwrap();
        write(&main, &format!("{text} "));
        write(&main, &text);
        let out = success(
            p.command()
                .env("JATO_KEY_ENV", "MACRO_SETTING")
                .env("MACRO_SETTING", value)
                .args(["run", "-q"]),
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(run("a"), "a\n");
    assert_eq!(run("b"), "b\n");
    let (hits, _) = p.during(|| assert_eq!(run("a"), "a\n"));
    assert_eq!(hits, 1);
}

#[test]
fn generated_sources_follow_their_build_script() {
    if !observes() {
        return;
    }
    let p = Project::new();
    write(
        p.root.join("leaf/build.rs"),
        r#"fn main() {
    println!("cargo:rerun-if-changed=seed.txt");
    let seed = std::fs::read_to_string("seed.txt").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/generated.rs"), format!("pub const SEED: &str = {:?};", seed.trim())).unwrap();
}"#,
    );
    write(p.root.join("leaf/seed.txt"), "alpha");
    write(
        p.root.join("leaf/src/value.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\")); pub fn value() -> String { SEED.to_owned() }",
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"alpha\n");
    // The generated file lives in the target directory, at a path that
    // differs in every one, and the library still moves between them.
    let other = p.temp.path().join("t2");
    let (hits, misses) = p.during(|| {
        let out = success(
            p.command()
                .env("CARGO_TARGET_DIR", &other)
                .args(["run", "-q"]),
        );
        assert_eq!(out.stdout, b"alpha\n");
    });
    assert_eq!(misses, 0, "{}", p.stats());
    assert!(hits >= 3, "{}", p.stats());
    write(p.root.join("leaf/seed.txt"), "beta");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"beta\n");
    assert_eq!(
        success(
            p.command()
                .env("CARGO_TARGET_DIR", &other)
                .args(["run", "-q"])
        )
        .stdout,
        b"beta\n"
    );
}

#[test]
fn features_and_compiler_settings_are_part_of_a_command() {
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
        p.root.join("leaf/src/value.rs"),
        "pub fn value() -> &'static str { if cfg!(feature = \"fast\") { \"fast\" } else if cfg!(debug_assertions) { \"debug\" } else { \"release\" } }",
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"debug\n");
    assert_eq!(
        p.run(&["run", "-q", "--features", "fast"]).stdout,
        b"fast\n"
    );
    assert_eq!(p.run(&["run", "-q", "--release"]).stdout, b"release\n");
    p.clean();
    let (_, misses) = p.during(|| {
        assert_eq!(p.run(&["run", "-q", "--release"]).stdout, b"release\n");
        assert_eq!(
            p.run(&["run", "-q", "--features", "fast"]).stdout,
            b"fast\n"
        );
        assert_eq!(p.run(&["run", "-q"]).stdout, b"debug\n");
    });
    if observes() {
        assert_eq!(misses, 0, "{}", p.stats());
    }
    // Flags reach the compiler as arguments and select other results.
    let flagged = success(
        p.command()
            .env("RUSTFLAGS", "--cfg feature=\"fast\"")
            .args(["run", "-q"]),
    );
    assert_eq!(flagged.stdout, b"fast\n");
}

#[test]
fn tests_and_executables_are_restored_with_their_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new();
    write(
        p.root.join("leaf/src/lib.rs"),
        "mod value; pub use value::value;\n#[test] fn works() { assert_eq!(value(), \"one:none\"); }",
    );
    let passed = |out: &Output| String::from_utf8_lossy(&out.stdout).contains("1 passed");
    assert!(passed(&p.run(&["test", "-p", "leaf"])));
    p.clean();
    let (hits, misses) = p.during(|| assert!(passed(&p.run(&["test", "-p", "leaf"]))));
    if observes() {
        assert_eq!(misses, 0, "{}", p.stats());
        assert!(hits >= 2);
    }
    p.run(&["build", "-q"]);
    let mode = fs::metadata(p.root.join("target/debug/app"))
        .unwrap()
        .permissions()
        .mode();
    p.clean();
    p.run(&["build", "-q"]);
    let restored = fs::metadata(p.root.join("target/debug/app")).unwrap();
    assert_eq!(restored.permissions().mode(), mode);
    assert_ne!(mode & 0o111, 0);
}

#[test]
fn the_same_compilation_started_twice_runs_once() {
    if !observes() {
        return;
    }
    let p = Project::new();
    // Slow enough to compile that the builds overlap.
    let mut body = String::from("pub fn value() -> String { let mut n = 0u64;\n");
    for i in 0..3000 {
        body.push_str(&format!("n = n.wrapping_mul(31).wrapping_add({i});\n"));
    }
    body.push_str("n.to_string() }");
    write(p.root.join("leaf/src/value.rs"), &body);
    success(p.command().args(["cargo", "generate-lockfile"]));
    let builds: Vec<_> = (0..4)
        .map(|i| {
            let mut cmd = p.command();
            cmd.env("CARGO_TARGET_DIR", p.temp.path().join(format!("t{i}")))
                .args(["build", "-q", "--release"]);
            cmd.spawn().unwrap()
        })
        .collect();
    for mut build in builds {
        assert!(build.wait().unwrap().success());
    }
    let outputs: Vec<_> = (0..4)
        .map(|i| {
            let app = p.temp.path().join(format!("t{i}/release/app"));
            success(&mut Command::new(app)).stdout
        })
        .collect();
    assert!(outputs.iter().all(|o| *o == outputs[0] && !o.is_empty()));
    // Two crates, each compiled once; the other six restored.
    let stats = p.stats();
    assert_eq!(stats["counters"]["stored"], 2, "{stats}");
    assert_eq!(stats["counters"]["hit"], 6, "{stats}");
}

#[test]
fn a_damaged_or_missing_object_is_compiled_again() {
    if !observes() {
        return;
    }
    let p = Project::new();
    p.run(&["build", "-q"]);
    let objects = p.temp.path().join("cache/objects");
    let mut files = Vec::new();
    for shard in fs::read_dir(&objects).unwrap() {
        let shard = shard.unwrap().path();
        if shard.is_dir() {
            files.extend(fs::read_dir(shard).unwrap().map(|f| f.unwrap().path()));
        }
    }
    assert!(files.len() >= 4, "{files:?}");
    // Remove one object and change the length of another.
    files.sort_by_key(|f| fs::metadata(f).unwrap().len());
    fs::remove_file(&files[0]).unwrap();
    let largest = files.last().unwrap();
    let mut permissions = fs::metadata(largest).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o600);
    fs::set_permissions(largest, permissions).unwrap();
    let mut bytes = fs::read(largest).unwrap();
    bytes.truncate(bytes.len() / 2);
    fs::write(largest, bytes).unwrap();
    p.clean();
    let (_, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    assert!(misses >= 1, "{}", p.stats());
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}

#[test]
fn the_budget_is_kept_with_the_cache_and_gc_evicts_to_a_size() {
    let p = Project::new();
    p.run(&["build", "-q"]);
    let limit = |args: &[&str]| String::from_utf8(p.run(args).stdout).unwrap();
    assert_eq!(limit(&["limit", "64MiB"]), "67108864 bytes\n");
    assert_eq!(
        String::from_utf8(success(p.command().env("JATO_MAX_SIZE", "1MiB").args(["limit"])).stdout)
            .unwrap(),
        "67108864 bytes\n"
    );
    if observes() {
        assert!(p.stats()["entries"].as_u64().unwrap() >= 2);
        // Everything stored so far fits, so nothing goes.
        assert_eq!(p.run(&["gc"]).stdout, b"Evicted 0 results\n");
        let out = String::from_utf8(p.run(&["gc", "0"]).stdout).unwrap();
        assert!(out.starts_with("Evicted "), "{out}");
        let stats = p.stats();
        assert_eq!(stats["entries"], 0, "{stats}");
        assert_eq!(stats["objects"], 0, "{stats}");
        assert_eq!(stats["stored_bytes"], 0, "{stats}");
        let left: usize = fs::read_dir(p.temp.path().join("cache/objects"))
            .unwrap()
            .map(|d| fs::read_dir(d.unwrap().path()).map_or(1, |files| files.count()))
            .sum();
        assert_eq!(left, 0);
        p.clean();
        assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    }
    assert_ne!(limit(&["limit", "--unset"]), "67108864 bytes\n");
    assert!(
        p.command()
            .args(["limit", "many"])
            .output()
            .unwrap()
            .status
            .code()
            == Some(1)
    );
}

#[test]
fn a_small_budget_keeps_only_recent_results() {
    if !observes() {
        return;
    }
    let p = Project::new();
    // Each revision embeds a megabyte the compressor cannot shrink.
    let revision = |n: u64| {
        let mut state = n.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut text = String::with_capacity(1 << 20);
        while text.len() < 1 << 20 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            text.push_str(&format!("{state:016x}"));
        }
        write(p.root.join("leaf/src/text file"), &text);
        text
    };
    p.run(&["limit", "12MiB"]);
    let first = revision(1);
    p.run(&["build", "-q"]);
    for n in 2..12 {
        revision(n);
        p.run(&["build", "-q"]);
    }
    let stats = p.stats();
    assert!(
        stats["stored_bytes"].as_u64().unwrap() <= 12 << 20,
        "{stats}"
    );
    assert!(
        stats["counters"]["evicted"].as_u64().unwrap() > 0,
        "{stats}"
    );
    // The latest revision is still there. The first is not, and builds.
    p.clean();
    let (_, misses) = p.during(|| drop(p.run(&["build", "-q"])));
    assert_eq!(misses, 0, "{}", p.stats());
    write(p.root.join("leaf/src/text file"), &first);
    let out = p.run(&["run", "-q"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with(&first[..64]));
}

#[test]
fn a_command_that_uses_the_network_is_run_and_never_stored() {
    if !observes() {
        return;
    }
    let p = Project::new();
    add_macros(&p.root);
    write(
        p.root.join("macros/src/lib.rs"),
        r#"use proc_macro::TokenStream;
#[proc_macro]
pub fn online(_: TokenStream) -> TokenStream {
    let _ = std::net::TcpStream::connect("127.0.0.1:9");
    "1".parse().unwrap()
}"#,
    );
    write(
        p.root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmacros={path='macros'}\n",
    );
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{}\", macros::online!()); }",
    );
    let out = success(p.command().env("JATO_LOG", "debug").args(["run", "-q"]));
    assert_eq!(out.stdout, b"1\n");
    assert!(
        decisions(&out, "app")
            .iter()
            .any(|d| d.contains("it used the network")),
        "{:?}",
        decisions(&out, "app")
    );
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"1\n"));
    // The macro library is restored. The crate that runs the macro is not.
    assert_eq!((hits, misses), (1, 1), "{}", p.stats());
}

#[test]
fn a_compiler_started_by_a_build_script_is_served_too() {
    let p = Project::new();
    // The way autocfg and many build scripts ask what the compiler supports.
    write(
        p.root.join("leaf/build.rs"),
        r#"use std::process::Command;
fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/probe.rs"), "pub fn probe() {}").unwrap();
    let rustc = std::env::var("RUSTC").unwrap();
    let mut cmd = match std::env::var("RUSTC_WRAPPER") {
        Ok(wrapper) if !wrapper.is_empty() => { let mut c = Command::new(wrapper); c.arg(rustc); c }
        _ => Command::new(rustc),
    };
    let status = cmd.args(["--crate-name", "probe", "--crate-type", "lib", "--emit", "metadata", "--out-dir", &out])
        .arg(format!("{out}/probe.rs")).status().unwrap();
    assert!(status.success());
    assert!(std::path::Path::new(&format!("{out}/libprobe.rmeta")).is_file());
    println!("cargo:rustc-cfg=probed");
}"#,
    );
    write(
        p.root.join("leaf/src/value.rs"),
        "pub fn value() -> &'static str { if cfg!(probed) { \"probed\" } else { \"plain\" } }",
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"probed\n");
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"probed\n"));
    if observes() {
        assert_eq!(misses, 0, "{}", p.stats());
        // The script, the probe it compiles, the library and the program.
        assert!(hits >= 4, "{}", p.stats());
    }
}

#[test]
fn compiler_queries_are_answered_as_the_compiler_answers_them() {
    let p = Project::new();
    let rustc = {
        let out = success(Command::new(env!("CARGO")).args(["--version", "--verbose"]));
        assert!(!out.stdout.is_empty());
        // Cargo names the compiler it runs in RUSTC for build scripts; ask
        // rustup's or the system's rustc the same way Cargo would.
        std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into())
    };
    let direct = success(Command::new(&rustc).arg("-vV"));
    for _ in 0..2 {
        let wrapped = success(p.command().args([&rustc, "-vV"]));
        assert_eq!(wrapped.stdout, direct.stdout);
        assert_eq!(wrapped.stderr, direct.stderr);
    }
    if observes() {
        assert_eq!(p.count("query_hit"), 1);
    }
    // A failing query is passed on as it is.
    let bad = p
        .command()
        .args([&rustc, "--print", "nonsense"])
        .output()
        .unwrap();
    let plain = Command::new(&rustc)
        .args(["--print", "nonsense"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), plain.status.code());
    assert_eq!(bad.stderr, plain.stderr);
}

#[test]
fn disabling_jato_runs_the_compiler_directly() {
    let p = Project::new();
    let out = success(
        p.command()
            .env("JATO_DISABLE", "1")
            .env("JATO_LOG", "debug")
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, b"one:none\n");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("jato ["));
    assert_eq!(p.hits() + p.misses(), 0);
}

#[test]
fn compressed_storage_restores_the_same_bytes() {
    if !observes() {
        return;
    }
    let p = Project::new();
    let packed = || {
        let mut cmd = p.command();
        cmd.env("JATO_STORAGE", "packed");
        cmd
    };
    assert_eq!(success(packed().args(["run", "-q"])).stdout, b"one:none\n");
    let original = fs::read(p.root.join("target/debug/app")).unwrap();
    p.clean();
    assert_eq!(success(packed().args(["run", "-q"])).stdout, b"one:none\n");
    assert_eq!(fs::read(p.root.join("target/debug/app")).unwrap(), original);
    let stats = p.stats();
    assert_eq!(stats["counters"]["hit"], 2, "{stats}");
    // The store holds less than the files it can reproduce.
    assert!(
        stats["stored_bytes"].as_u64().unwrap() < stats["artifact_bytes"].as_u64().unwrap(),
        "{stats}"
    );
    assert!(
        packed()
            .env("JATO_STORAGE", "bogus")
            .arg("stats")
            .output()
            .unwrap()
            .status
            .code()
            == Some(1)
    );
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
fn another_wrapper_named_for_the_workspace_still_runs() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new();
    let helper = p.temp.path().join("my-driver");
    let mark = p.temp.path().join("driver-ran");
    // The driver leaves a file behind. That is part of what running the
    // command does, so a reused result leaves it too.
    let driver = |note: &str| {
        write(
            &helper,
            &format!("#!/bin/sh\nprintf '{note} %s\\n' \"$1\" > \"$DRIVER_MARK\"\nexec \"$@\"\n"),
        );
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    };
    driver("ran");
    let mut paths = vec![p.temp.path().to_owned()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let build = || {
        success(
            p.command()
                .env("PATH", std::env::join_paths(&paths).unwrap())
                .env("RUSTC_WORKSPACE_WRAPPER", "my-driver")
                .env("DRIVER_MARK", &mark)
                .args(["run", "-q"]),
        )
    };
    assert_eq!(build().stdout, b"one:none\n");
    let ran = fs::read_to_string(&mark).unwrap();
    assert!(
        ran.starts_with("ran ") && ran.trim_end().ends_with("rustc"),
        "{ran}"
    );
    if observes() {
        fs::remove_file(&mark).unwrap();
        p.clean();
        let (hits, misses) = p.during(|| assert_eq!(build().stdout, b"one:none\n"));
        assert_eq!((hits, misses), (2, 0), "{}", p.stats());
        assert_eq!(fs::read_to_string(&mark).unwrap(), ran);
        // The driver's own contents are an input of the command it ran.
        // Cargo does not use it for the library, which is not a member.
        driver("again");
        p.clean();
        let (hits, misses) = p.during(|| assert_eq!(build().stdout, b"one:none\n"));
        assert_eq!((hits, misses), (1, 1), "{}", p.stats());
        assert!(fs::read_to_string(&mark).unwrap().starts_with("again "));
    }
}

#[test]
fn a_program_found_earlier_on_path_is_noticed() {
    use std::os::unix::fs::PermissionsExt;
    if !observes() {
        return;
    }
    let p = Project::new();
    let Some(system_cc) = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("cc"))
        .find(|cc| cc.is_file())
    else {
        return;
    };
    let build = |extra: Option<&Path>| {
        let mut paths: Vec<PathBuf> = extra.into_iter().map(Path::to_owned).collect();
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut cmd = p.command();
        // Link through the C compiler found on PATH.
        cmd.env("PATH", std::env::join_paths(paths).unwrap())
            .env("RUSTFLAGS", format!("-Clinker=cc {}", system_linker()))
            .args(["run", "-q"]);
        success(&mut cmd)
    };
    assert_eq!(build(None).stdout, b"one:none\n");
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(build(None).stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    // An unrelated directory in front changes nothing.
    let empty = p.temp.path().join("empty-bin");
    fs::create_dir_all(&empty).unwrap();
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(build(Some(&empty)).stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    // Another `cc` in front is a different linker for the program. The
    // library never ran one and is restored.
    let shim = p.temp.path().join("shim-bin");
    write(
        shim.join("cc"),
        &format!("#!/bin/sh\nexec {} \"$@\"\n", system_cc.display()),
    );
    fs::set_permissions(shim.join("cc"), fs::Permissions::from_mode(0o755)).unwrap();
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(build(Some(&shim)).stdout, b"one:none\n"));
    assert_eq!((hits, misses), (1, 1), "{}", p.stats());
}

#[test]
fn kept_cargo_state_delivers_to_new_targets_and_rechecks_edits() {
    let p = Project::new();
    let a = p.temp.path().join("delivery a");
    let b = p.temp.path().join("delivery b");
    let command = |target: &Path| {
        let mut cmd = p.command();
        cmd.env("CARGO_TARGET_DIR", target).arg("--keep-build");
        cmd
    };
    let metadata = |cmd: &mut Command| -> Value {
        serde_json::from_slice(
            &success(cmd.args(["cargo", "metadata", "--format-version=1", "--no-deps"])).stdout,
        )
        .unwrap()
    };
    let layout = metadata(&mut command(&a));
    let Some(build) = layout["build_directory"].as_str().map(Path::new) else {
        // Cargo before 1.91 uses its ordinary target layout.
        assert_eq!(
            success(command(&a).args(["run", "-q"])).stdout,
            b"one:none\n"
        );
        return;
    };
    assert!(build.starts_with(p.temp.path().join("cargo/jato-build")));
    assert_eq!(
        metadata(&mut command(&b))["build_directory"],
        layout["build_directory"]
    );
    let explicit = p.temp.path().join("explicit state");
    assert_eq!(
        metadata(command(&a).env("CARGO_BUILD_BUILD_DIR", &explicit))["build_directory"],
        explicit.to_str().unwrap()
    );
    let other = Project::new();
    let other_layout = metadata(command(&a).current_dir(&other.root));
    assert_ne!(other_layout["build_directory"], layout["build_directory"]);
    let build_at = |dir: &Path| {
        let output = success(command(dir).args(["build", "--message-format=json"]));
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|message| message["reason"] == "compiler-artifact")
            .map(|message| message["fresh"].as_bool().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(build_at(&a), [false, false]);
    let original = fs::read(a.join("debug/app")).unwrap();
    fs::remove_dir_all(&a).unwrap();
    assert_eq!(build_at(&b), [true, true]);
    assert_eq!(fs::read(b.join("debug/app")).unwrap(), original);
    write(p.root.join("leaf/src/text file"), "two");
    assert_eq!(
        success(command(&a).args(["run", "-q"])).stdout,
        b"two:none\n"
    );
    success(command(&a).args(["cargo", "clean"]));
    assert_eq!(build_at(&b), [false, false]);
}

#[test]
fn fast_development_profile_keeps_project_settings_and_argument_boundaries() {
    let p = Project::new();
    let manifest = p.root.join("Cargo.toml");
    let mut contents = fs::read_to_string(&manifest).unwrap();
    contents.push_str("\n[profile.dev]\nopt-level=1\n");
    write(&manifest, &contents);
    write(
        p.root.join("src/main.rs"),
        "fn main(){println!(\"{} {:?}\",cfg!(debug_assertions), std::env::args().skip(1).collect::<Vec<_>>());}",
    );
    let build = p.run(&["dev", "--message-format=json"]);
    let artifacts: Vec<_> = String::from_utf8(build.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["reason"] == "compiler-artifact")
        .collect();
    let app = artifacts
        .iter()
        .find(|v| v["target"]["name"] == "app")
        .unwrap();
    let leaf = artifacts
        .iter()
        .find(|v| v["target"]["name"] == "leaf")
        .unwrap();
    assert_eq!(app["profile"]["debuginfo"], "line-tables-only");
    assert_eq!(leaf["profile"]["debuginfo"], 0);
    assert_eq!(app["profile"]["opt_level"], "1");
    assert_eq!(app["profile"]["debug_assertions"], true);
    assert!(p.root.join("target/jato-dev/app").is_file());
    assert_eq!(
        p.run(&["dev", "run", "-q", "--", "--profile=release", "two words"])
            .stdout,
        b"true [\"--profile=release\", \"two words\"]\n"
    );
    assert_eq!(
        p.run(&["dev", "cargo", "--offline", "run", "-qr"]).stdout,
        b"false []\n"
    );
    assert!(p.root.join("target/release/app").is_file());
    p.run(&["dev", "test", "-q", "--", "--list"]);
    p.run(&["dev", "cargo", "clean"]);
    assert!(!p.root.join("target/jato-dev/app").exists());
    assert!(p.root.join("target/release/app").is_file());
}

#[test]
fn watcher_checks_edited_package_then_workspace_and_ignores_build_outputs() {
    check_watcher(false);
}

#[test]
fn fast_profile_watcher_runs_focused_and_workspace_checks() {
    check_watcher(true);
}

fn check_watcher(fast: bool) {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let p = Project::new();
    let manifest = p.root.join("Cargo.toml");
    let mut contents = fs::read_to_string(&manifest).unwrap();
    contents.push_str("\n[workspace]\nmembers=['leaf','plugins/*']\n");
    write(
        p.root.join("plugins/base/Cargo.toml"),
        "[package]\nname='base'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(p.root.join("plugins/base/src/lib.rs"), "");
    write(manifest, &contents);
    p.run(&["check", "--workspace"]); // establish lockfile and initial target
    let mut cmd = p.command();
    if fast {
        cmd.arg("dev");
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
        let until = Instant::now() + Duration::from_secs(60);
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
    if fast {
        assert!(p.root.join("target/jato-dev").is_dir());
    }
    write(p.root.join("target/ignored"), "build output");
    assert!(recv.recv_timeout(Duration::from_millis(180)).is_err());
    write(p.root.join("leaf/src/text file"), "edited");
    wait("cargo check -p leaf");
    wait("cargo check --workspace");
    wait("finished in");
    // A populated directory can be renamed in before any watch on its
    // children exists. Preserve its ignore rules, and follow later moves.
    let stage = p.temp.path().join("incoming");
    write(stage.join(".ignore"), "*.tmp\n!important.tmp\n");
    write(stage.join("nested/ignored.tmp"), "before");
    write(stage.join("nested/important.tmp"), "before");
    let arrived = p.root.join("leaf/assets");
    fs::rename(&stage, &arrived).unwrap();
    let finish_leaf = || {
        wait("cargo check -p leaf");
        wait("cargo check --workspace");
        wait("finished in");
    };
    finish_leaf();
    write(arrived.join("nested/ignored.tmp"), "ignored");
    assert!(recv.recv_timeout(Duration::from_millis(180)).is_err());
    write(arrived.join("nested/important.tmp"), "included");
    finish_leaf();
    let moved = p.root.join("leaf/moved");
    fs::rename(&arrived, &moved).unwrap();
    finish_leaf();
    write(moved.join("nested/important.tmp"), "edited after move");
    finish_leaf();
    // An arriving workspace member can introduce a dependency outside
    // every previous watch root. Cargo must re-resolve this topology.
    let external = p.temp.path().join("external");
    write(
        external.join("Cargo.toml"),
        "[package]\nname='external'\nversion='0.1.0'\nedition='2021'\n",
    );
    let external_src = external.join("src/lib.rs");
    write(&external_src, "pub fn source(){}");
    write(
        stage.join("Cargo.toml"),
        &format!(
            "[package]\nname='discovered'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nexternal={{path={external:?}}}\n"
        ),
    );
    write(
        stage.join("src/lib.rs"),
        "pub fn external(){external::source();}",
    );
    fs::rename(&stage, p.root.join("plugins/new")).unwrap();
    wait("Checking discovered");
    wait("finished in");
    // Let Cargo's own lockfile update finish before the external edit.
    let end = Instant::now() + Duration::from_secs(20);
    while recv.recv_timeout(Duration::from_millis(180)).is_ok() {
        assert!(Instant::now() < end);
    }
    write(&external_src, "pub fn source(){missing_external();}");
    wait("error[E0425]");
    wait("exit 101");
    write(&external_src, "pub fn source(){}");
    wait("exit 0");
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGINT);
    }
    assert_eq!(child.0.wait().unwrap().code(), Some(130));
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
            std::thread::sleep(Duration::from_secs(30));
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
        let end = Instant::now() + Duration::from_secs(60);
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

/// Link with mold, which replaces its output by moving the previous one
/// aside, and finishes in a process that outlives the one rustc waits for.
#[test]
fn mold_links_are_restored_and_relinked_over_earlier_output() {
    let Some(mold) = std::env::var_os("JATO_TEST_MOLD") else {
        return;
    };
    let p = Project::new();
    add_macros(&p.root);
    write(
        p.root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nleaf={path='leaf'}\nmacros={path='macros'}\n",
    );
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{} {}\", leaf::value(), macros::hidden!()); }",
    );
    write(p.root.join("hidden.txt"), "h");
    let bin = p.temp.path().join("mold-bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&mold, bin.join("ld.mold")).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let build = || {
        let mut cmd = p.command();
        cmd.env("PATH", std::env::join_paths(&paths).unwrap())
            .env(
                "RUSTFLAGS",
                format!("-Clinker=cc {} -Clink-arg=-fuse-ld=mold", system_linker()),
            )
            .args(["run", "-q"]);
        success(&mut cmd)
    };
    assert_eq!(build().stdout, b"one:none h\n");
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(build().stdout, b"one:none h\n"));
    assert_eq!((hits, misses), (3, 0), "{}", p.stats());
    // Relink over the executable that is already there, twice, and return.
    for text in ["two", "three", "one"] {
        write(p.root.join("leaf/src/text file"), text);
        assert_eq!(build().stdout, format!("{text}:none h\n").as_bytes());
    }
    let stats = p.stats();
    assert_eq!(
        stats["counters"]["uncacheable"].as_u64().unwrap_or(0),
        0,
        "{stats}"
    );
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(build().stdout, b"one:none h\n"));
    assert_eq!((hits, misses), (3, 0), "{}", p.stats());
}

/// Nightly Cargo keeps a library's metadata beside it and passes unstable
/// options. None of that needs special treatment.
#[test]
fn nightly_split_metadata_and_unstable_options_are_ordinary_commands() {
    let Some(rustc) = std::env::var_os("JATO_TEST_NIGHTLY") else {
        return;
    };
    let cargo = Path::new(&rustc).with_file_name("cargo");
    let p = Project::new();
    let build = |flags: &str| {
        let mut cmd = p.command();
        cmd.env("CARGO", &cargo)
            .env("RUSTC", &rustc)
            .env("RUSTFLAGS", flags)
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["run", "-q"]);
        success(&mut cmd)
    };
    assert_eq!(build("-Zthreads=4").stdout, b"one:none\n");
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(build("-Zthreads=4").stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    // An option Jato has never heard of is simply part of the command.
    assert_eq!(build("-Zthreads=4 -Zmacro-backtrace").stdout, b"one:none\n");
    p.clean();
    let (hits, misses) =
        p.during(|| assert_eq!(build("-Zthreads=4 -Zmacro-backtrace").stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}
