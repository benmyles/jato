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
    /// Whether builds are looked up and recorded as wholes. Most tests
    /// are about single compilations and say how many they expect.
    whole: bool,
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
        Self {
            root,
            temp,
            whole: false,
        }
    }

    /// A project whose builds are recorded and restored as wholes, as
    /// they are for anyone who has not asked otherwise.
    fn whole() -> Self {
        Self {
            whole: true,
            ..Self::new()
        }
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
            .env_remove("JATO_PATH")
            // Tests edit and build within seconds and expect each result to
            // be recorded. The test of editing spells says otherwise.
            .env("JATO_EDIT_WINDOW", "0")
            // What is recorded must not depend on what else this machine
            // is doing. The test of a busy machine says otherwise.
            .env("JATO_BUSY", "0")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTC_BOOTSTRAP")
            .env_remove("LEAF_ENV");
        if !self.whole {
            cmd.env("JATO_WHOLE_BUILDS", "0");
        }
        // To see why a test went as it did, name a file here.
        if let Some(log) = std::env::var_os("JATO_TEST_LOG") {
            cmd.env("JATO_LOG", log);
        }
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        success(self.command().args(args))
    }

    /// How many of the crates built in `root` Cargo itself would compile
    /// again, asked without Jato.
    fn stale_for_cargo(&self, root: &Path, target: &Path) -> usize {
        let mut cmd = self.command_in(root);
        let output = success(
            cmd.env("JATO_DISABLE", "1")
                .env("CARGO_TARGET_DIR", target)
                .args(["cargo", "build", "--message-format=json"]),
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|message| message["reason"] == "compiler-artifact")
            .filter(|message| message["fresh"] == false)
            .count()
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
    let p = Project::whole();
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
    assert_eq!(warnings(&p.run(&args)), first);
    if observes() {
        // The whole check was recorded, so it is put back as one.
        assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
        // And each crate is, when Cargo asks for them one at a time.
        p.clean();
        let (hits, _) = p.during(|| {
            let out = success(p.command().env("JATO_WHOLE_BUILDS", "0").args(args));
            assert_eq!(warnings(&out), first);
        });
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

/// A program that says where it was built is its own in every checkout.
/// The rest of the build is not: another checkout starts from the build
/// with that program to make again.
#[test]
fn a_build_whose_program_names_its_checkout_is_started_from_in_another() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    let main = "fn main() { println!(\"{} {}\", env!(\"CARGO_MANIFEST_DIR\"), leaf::value()); }";
    write(p.root.join("src/main.rs"), main);
    let program = |root: &Path| {
        let out = Command::new(root.join("target/debug/app"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let says = |root: &Path| format!("{} one:none\n", root.display());
    p.run(&["build"]);
    assert_eq!(program(&p.root), says(&p.root));
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // Where it was made it is put back whole.
    p.clean();
    p.run(&["build"]);
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(program(&p.root), says(&p.root));
    // Somewhere else the library is put back and the program compiled.
    let checkout = |name: &str| {
        let root = p.temp.path().join(name);
        populate(&root);
        write(root.join("src/main.rs"), main);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
        root
    };
    let second = checkout("second/checkout");
    let out = success(
        p.command_in(&second)
            .env("JATO_LOG", "debug")
            .args(["build"]),
    );
    let said = decisions(&out, "cargo");
    assert!(
        said.iter().any(|d| d.contains("1 of its 2 compilations")
            && d.contains("names where it was made")),
        "{said:?}"
    );
    assert!(
        decisions(&out, "leaf").is_empty(),
        "{:?}",
        decisions(&out, "leaf")
    );
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
    assert_eq!(program(&second), says(&second));
    assert_eq!(p.stale_for_cargo(&second, &second.join("target")), 0);
    // That build is put back whole there, and the first one where it was.
    fs::remove_dir_all(second.join("target")).unwrap();
    success(p.command_in(&second).args(["build"]));
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    assert_eq!(program(&second), says(&second));
    p.clean();
    p.run(&["build"]);
    assert_eq!(p.count("build_hit"), 3, "{}", p.stats());
    assert_eq!(program(&p.root), says(&p.root));
    // A third checkout, with the library changed as well.
    let third = checkout("third");
    write(third.join("leaf/src/text file"), "two");
    success(p.command_in(&third).args(["build"]));
    assert_eq!(p.count("build_near"), 2, "{}", p.stats());
    assert_eq!(program(&third), format!("{} two:none\n", third.display()));
    // A check first, and the build after it in the same target directory:
    // recorded as resting on the check, with the check's files in it.
    // Most of them are the check's, as they are in a workspace whose
    // build scripts write a great deal.
    let q = Project::whole();
    let script = r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    for n in 0..60 {
        std::fs::write(format!("{out}/made-{n}.txt"), n.to_string()).unwrap();
    }
    println!("cargo:rerun-if-changed=build.rs");
}
"#;
    write(q.root.join("build.rs"), script);
    write(q.root.join("src/main.rs"), main);
    q.run(&["check"]);
    q.run(&["build"]);
    assert_eq!(q.count("build_stored"), 2, "{}", q.stats());
    // Where only the check has been put back, the programs have not been
    // built: the build starts from the recorded one, and is not left to
    // Cargo as one in a target directory that is built already.
    let fourth = q.temp.path().join("fourth");
    populate(&fourth);
    write(fourth.join("build.rs"), script);
    write(fourth.join("src/main.rs"), main);
    fs::copy(q.root.join("Cargo.lock"), fourth.join("Cargo.lock")).unwrap();
    success(q.command_in(&fourth).args(["check"]));
    assert_eq!(q.count("build_hit"), 1, "{}", q.stats());
    success(q.command_in(&fourth).args(["build"]));
    assert_eq!(q.count("build_left_to_cargo"), 0, "{}", q.stats());
    assert_eq!(q.count("build_near"), 1, "{}", q.stats());
    assert_eq!(q.count("build_stored"), 3, "{}", q.stats());
    assert_eq!(program(&fourth), says(&fourth));
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

/// A project in `dir/ws` that takes a crate from beside itself, `dir/shared`,
/// which says `said`.
fn beside(dir: &Path, said: &str) -> PathBuf {
    let root = dir.join("ws");
    write(
        root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nshared={path='../shared'}\n",
    );
    write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", shared::said()); }",
    );
    write(
        dir.join("shared/Cargo.toml"),
        "[package]\nname='shared'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        dir.join("shared/src/lib.rs"),
        &format!("pub fn said() -> &'static str {{ {said:?} }}"),
    );
    root
}

#[test]
fn a_crate_beside_a_checkout_is_looked_at_beside_each_checkout() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    let binary = |root: &Path| {
        let out = Command::new(root.join("target/debug/app"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let first = beside(&p.temp.path().join("first"), "one");
    success(p.command_in(&first).args(["build"]));
    assert_eq!(binary(&first), "one\n");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    let lock = fs::read(first.join("Cargo.lock")).unwrap();
    let copy = |name: &str, said: &str| {
        let root = beside(&p.temp.path().join(name), said);
        fs::write(root.join("Cargo.lock"), &lock).unwrap();
        root
    };
    // A checkout with the same crate beside it is the same build.
    let same = copy("same", "one");
    success(p.command_in(&same).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(binary(&same), "one\n");
    // One with another crate beside it is not, though the first
    // checkout's neighbour is still as it was.
    let other = copy("other", "two");
    success(p.command_in(&other).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(binary(&other), "two\n");
}

#[test]
fn a_file_included_from_beside_a_checkout_is_looked_at_beside_each_checkout() {
    let p = Project::named("first/checkout");
    // A crate that takes a file from outside its workspace.
    let outside = |root: &Path, motto: &str| {
        write(
            root.join("src/main.rs"),
            "fn main() { println!(\"{} {}\", leaf::value(), include_str!(\"../../motto.txt\")); }",
        );
        write(root.parent().unwrap().join("motto.txt"), motto);
    };
    outside(&p.root, "onward");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none onward\n");
    let copy = |name: &str, motto: &str| {
        let root = p.temp.path().join(name).join("checkout");
        populate(&root);
        outside(&root, motto);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
        root
    };
    // Where the file says something else the crate is compiled, though
    // the file beside the first checkout is as it was.
    let second = copy("second", "sideways");
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&second).args(["run", "-q"]));
        assert_eq!(out.stdout, b"one:none sideways\n");
    });
    assert_eq!((hits, misses), (1, 1), "{}", p.stats());
    // Where it says the same, the result is the same.
    let third = copy("third", "onward");
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&third).args(["run", "-q"]));
        assert_eq!(out.stdout, b"one:none onward\n");
    });
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}

#[test]
fn settings_above_a_checkout_are_looked_for_above_each_checkout() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    let copy = |root: &Path| {
        populate(root);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    };
    // A checkout further down, with nothing new above it, is given the
    // build.
    let plain = p.temp.path().join("plain/further/down/checkout");
    copy(&plain);
    success(p.command_in(&plain).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // One below a directory with settings for Cargo is another build.
    let set = p.temp.path().join("set/further/checkout");
    copy(&set);
    write(
        p.temp.path().join("set/.cargo/config.toml"),
        "[env]\nLEAF_ENV = \"from above\"\n",
    );
    assert_eq!(
        success(p.command_in(&set).args(["run", "-q"])).stdout,
        b"one:from above\n"
    );
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

/// A program finds what it keeps for the person in the home directory by
/// the home directory's name: git its `.gitconfig`, a tool the settings
/// it reads. From a checkout further down, the same steps up would lead
/// somewhere else, and nothing took them.
#[test]
fn settings_kept_in_the_home_directory_are_found_there_from_any_checkout() {
    if !observes() {
        return;
    }
    let p = Project {
        whole: true,
        ..Project::named("home/app")
    };
    let home = p.temp.path().join("home");
    let at_home = |root: &Path| {
        let mut cmd = p.command_in(root);
        cmd.env("HOME", &home);
        // The toolchains stay where they are.
        if std::env::var_os("RUSTUP_HOME").is_none()
            && let Some(real) = std::env::var_os("HOME")
        {
            cmd.env("RUSTUP_HOME", Path::new(&real).join(".rustup"));
        }
        cmd
    };
    let script = |root: &Path| {
        write(
            root.join("leaf/build.rs"),
            r#"fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let home = std::env::var("HOME").unwrap();
    let kept = std::fs::read_to_string(format!("{home}/.leafrc")).unwrap_or_default();
    println!("cargo:rustc-env=LEAF_ENV={}", kept.trim());
}
"#,
        );
    };
    script(&p.root);
    write(home.join(".leafrc"), "kept\n");
    assert_eq!(
        success(at_home(&p.root).args(["run", "-q"])).stdout,
        b"one:kept\n"
    );
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    let copy = |root: &Path| {
        populate(root);
        script(root);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    };
    // Further down in the home directory, and outside it.
    let down = home.join("work/further/down/app");
    copy(&down);
    assert_eq!(
        success(at_home(&down).args(["run", "-q"])).stdout,
        b"one:kept\n"
    );
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    let outside = p.temp.path().join("elsewhere/app");
    copy(&outside);
    assert_eq!(
        success(at_home(&outside).args(["run", "-q"])).stdout,
        b"one:kept\n"
    );
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    // What is kept there is still what the build read.
    write(home.join(".leafrc"), "new\n");
    let other = home.join("other/app");
    copy(&other);
    assert_eq!(
        success(at_home(&other).args(["run", "-q"])).stdout,
        b"one:new\n"
    );
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
}

#[test]
fn a_directory_named_like_one_at_the_top_of_the_filesystem_was_not_searched_for() {
    if !observes() {
        return;
    }
    // Every program looks in `/lib`. A crate kept in `lib` does not make
    // that a search for `lib` in each directory above the workspace, which
    // would have had to look in the ones between as well.
    let p = Project::whole();
    fs::rename(p.root.join("leaf"), p.root.join("lib")).unwrap();
    write(
        p.root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nleaf={path='lib'}\n",
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

#[test]
fn a_build_is_put_back_after_the_checkout_that_made_it_is_gone() {
    if !observes() {
        return;
    }
    // Cargo asks about every directory on the way to a workspace. A
    // build made in one that has since been deleted, with the directory
    // it was in, is none the worse for that.
    let p = Project::whole();
    let first = p.temp.path().join("first/deep/checkout");
    populate(&first);
    let out = success(p.command_in(&first).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    let second = p.temp.path().join("second/checkout");
    populate(&second);
    fs::copy(first.join("Cargo.lock"), second.join("Cargo.lock")).unwrap();
    fs::remove_dir_all(p.temp.path().join("first")).unwrap();
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

/// Whether git is installed.
fn has_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Run git in `dir` as someone with a name, whatever this machine's own
/// settings for git are.
fn git(dir: &Path, args: &[&str]) {
    success(
        Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args([
                "-c",
                "user.name=Jato",
                "-c",
                "user.email=jato@example.invalid",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args),
    );
}

/// Make `p` a repository with everything committed, and give its root
/// package a build script that names no inputs: Cargo then asks git which
/// files the package has, to compare their times.
fn commit_all(p: &Project) {
    write(p.root.join("build.rs"), "fn main() {}\n");
    write(p.root.join(".gitignore"), "/target\n");
    p.run(&["cargo", "generate-lockfile"]);
    git(&p.root, &["init", "-q"]);
    git(&p.root, &["add", "-A"]);
    git(&p.root, &["commit", "-q", "-m", "first"]);
}

#[test]
fn a_build_is_put_back_in_another_worktree_and_after_a_commit() {
    if !observes() || !has_git() {
        return;
    }
    let p = Project::whole();
    commit_all(&p);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 0, "{}", p.stats());
    // What git keeps about the repository changes with a commit, with the
    // branch, and when it only looks. The files that were built do not.
    git(&p.root, &["commit", "-q", "--allow-empty", "-m", "nothing"]);
    git(&p.root, &["checkout", "-q", "-b", "topic"]);
    git(&p.root, &["status"]);
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // A worktree keeps its part of the repository somewhere else, under
    // the first checkout, and has a file where that has a directory.
    let tree = p.temp.path().join("trees/second");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            tree.to_str().unwrap(),
        ],
    );
    let out = success(p.command_in(&tree).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    // A clone is a repository of its own.
    let clone = p.temp.path().join("clone");
    git(
        p.temp.path(),
        &["clone", "-q", p.root.to_str().unwrap(), "clone"],
    );
    let out = success(p.command_in(&clone).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 3, "{}", p.stats());
    // So is a worktree of a repository that has no files checked out.
    let bare = p.temp.path().join("bare.git");
    git(
        p.temp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            p.root.to_str().unwrap(),
            "bare.git",
        ],
    );
    let third = p.temp.path().join("third");
    git(
        &bare,
        &["worktree", "add", "-q", third.to_str().unwrap(), "topic"],
    );
    let out = success(p.command_in(&third).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 4, "{}", p.stats());
    // What is committed in one worktree and built there from nothing is
    // found by the next one to check it out.
    edit_leaf(&tree, "two");
    git(&tree, &["commit", "-q", "-a", "-m", "second"]);
    success(p.command_in(&tree).args(["cargo", "clean"]));
    let out = success(p.command_in(&tree).args(["run", "-q"]));
    assert_eq!(out.stdout, b"two\n");
    assert_eq!(p.count("build_hit"), 4, "{}", p.stats());
    git(&p.root, &["checkout", "-q", "--detach", "other"]);
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"two\n");
    assert_eq!(p.count("build_hit"), 5, "{}", p.stats());
}

#[test]
fn what_cargo_passes_over_in_a_directory_it_lists_keeps_no_build_away() {
    if !observes() || !has_git() {
        return;
    }
    // To ask git for the files of a package, Cargo lists the top of the
    // repository and the directories of the package. It takes nothing from
    // the names there: not from a file git does not track, not from the
    // directory an earlier build was made in.
    let p = Project::whole();
    commit_all(&p);
    // The build script asks where its package really is, as one does that
    // goes on to read files beside it. That is a question about each
    // directory on the way, and no look at what they hold.
    write(
        p.root.join("build.rs"),
        r#"fn main() {
    let here = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let real = std::fs::canonicalize(format!("{here}/leaf/..")).unwrap();
    assert!(std::fs::metadata(&real).unwrap().is_dir());
}
"#,
    );
    git(&p.root, &["commit", "-q", "-a", "-m", "ask"]);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    let tree = p.temp.path().join("trees/second");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            tree.to_str().unwrap(),
        ],
    );
    write(tree.join("notes.txt"), "to self");
    write(tree.join("scratch/more/notes.txt"), "to self");
    write(tree.join("leaf/src/.swap"), "");
    let out = success(p.command_in(&tree).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // Another target directory, with the first still where it was.
    let elsewhere = p.temp.path().join("another-target");
    let out = success(
        p.command()
            .env("CARGO_TARGET_DIR", &elsewhere)
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    // A copy that is no repository at all: Cargo lists other directories
    // there, for the same files.
    let export = p.temp.path().join("export");
    fs::create_dir_all(&export).unwrap();
    let archive = success(
        Command::new("git")
            .current_dir(&p.root)
            .args(["archive", "HEAD"]),
    );
    let mut unpack = Command::new("tar")
        .current_dir(&export)
        .arg("-x")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(unpack.stdin.as_mut().unwrap(), &archive.stdout).unwrap();
    assert!(unpack.wait_with_output().unwrap().status.success());
    let out = success(p.command_in(&export).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 3, "{}", p.stats());
}

#[test]
fn a_package_added_where_a_workspace_looks_for_packages_is_built() {
    if !observes() {
        return;
    }
    // A workspace that finds its packages by a pattern.
    let p = Project::whole();
    let ready = |root: &Path| {
        fs::remove_dir_all(root.join("src")).unwrap();
        write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\n    # Wherever they are.\n    \"crates/*\",\n]\n",
        );
        write(
            root.join("crates/first/Cargo.toml"),
            "[package]\nname='first'\nversion='0.1.0'\nedition='2021'\n",
        );
        write(root.join("crates/first/src/main.rs"), "fn main() {}");
    };
    let package = |root: &Path, at: &str, name: &str| {
        write(
            root.join(at).join("Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
        );
        write(root.join(at).join("src/main.rs"), "fn main() {}");
    };
    ready(&p.root);
    p.run(&["build"]);
    assert!(p.root.join("target/debug/first").exists());
    let copy = |name: &str| {
        let root = p.temp.path().join(name).join("checkout");
        populate(&root);
        ready(&root);
        root
    };
    // The same again is put back.
    let same = copy("same");
    fs::copy(p.root.join("Cargo.lock"), same.join("Cargo.lock")).unwrap();
    success(p.command_in(&same).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // A package beside the first one, before Cargo has had a chance to
    // put it in the lock file.
    let second = copy("second");
    fs::copy(p.root.join("Cargo.lock"), second.join("Cargo.lock")).unwrap();
    package(&second, "crates/second", "second");
    success(p.command_in(&second).args(["build"]));
    assert!(second.join("target/debug/second").exists());
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

#[test]
fn a_build_cargo_makes_for_one_of_its_subcommands_is_cargo_s_own() {
    if !observes() || !has_git() {
        return;
    }
    // For clippy, Cargo turns into a small program of clippy's that starts
    // Cargo again. That Cargo reads the repository and lists directories
    // like any other, and what it starts as compilers are its own.
    // Only with the toolchain's own clippy: another toolchain's, found
    // along PATH, would compile with another compiler.
    if !Path::new(env!("CARGO"))
        .with_file_name("clippy-driver")
        .exists()
    {
        return;
    }
    let p = Project::whole();
    commit_all(&p);
    success(p.command().args(["clippy"]));
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    let tree = p.temp.path().join("trees/second");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            tree.to_str().unwrap(),
        ],
    );
    write(tree.join("notes.txt"), "to self");
    success(p.command_in(&tree).args(["clippy"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // And a worktree with a change starts from that build.
    let third = p.temp.path().join("trees/third");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "more",
            third.to_str().unwrap(),
        ],
    );
    edit_leaf(&third, "two");
    make_old(&third.join("leaf/src/value.rs"));
    success(p.command_in(&third).args(["clippy"]));
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
}

#[test]
fn a_build_in_a_target_made_without_jato_is_said_to_be_of_use_only_there() {
    if !observes() {
        return;
    }
    // Someone who has been building with Cargo alone starts using Jato.
    // Cargo finds everything in the target directory up to date, and
    // nothing is compiled that Jato could keep.
    let p = Project::whole();
    success(
        p.command()
            .env("JATO_DISABLE", "1")
            .args(["cargo", "build"]),
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    let said = p.stats()["only_there"][1].as_str().map(str::to_owned);
    let said = said.unwrap_or_default();
    assert!(said.contains("in the target directory already"), "{said}");
    let shown = String::from_utf8(p.run(&["stats"]).stdout).unwrap();
    assert!(shown.contains("only put back where it was made"), "{shown}");
    // Another checkout gets nothing from it, and makes a build that any
    // checkout can have.
    let second = p.temp.path().join("second/checkout");
    populate(&second);
    fs::copy(p.root.join("Cargo.lock"), second.join("Cargo.lock")).unwrap();
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 0, "{}", p.stats());
    assert!(p.stats()["only_there"].is_null(), "{}", p.stats());
    let third = p.temp.path().join("third/checkout");
    populate(&third);
    fs::copy(p.root.join("Cargo.lock"), third.join("Cargo.lock")).unwrap();
    let out = success(p.command_in(&third).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

#[test]
fn a_build_in_a_target_another_command_used_is_put_back_where_that_command_s_is() {
    if !observes() {
        return;
    }
    // A check and a build share what Cargo builds for the machine doing
    // the building: build scripts, and what running them leaves. A build
    // made after a check, in the same target directory, has read some of
    // what the check left there, and that names the checkout.
    let p = Project::whole();
    let ready = |root: &Path| {
        write(
            root.join("build.rs"),
            r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/made.rs"), "const MADE: u32 = 7;").unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
"#,
        );
        write(
            root.join("src/main.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/made.rs\"));\nfn main() { println!(\"{MADE} {}\", leaf::value()); }",
        );
    };
    ready(&p.root);
    p.run(&["check"]);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"7 one:none\n");
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
    // Another checkout, where the check is put back and then the build.
    let second = p.temp.path().join("second/checkout");
    populate(&second);
    ready(&second);
    fs::copy(p.root.join("Cargo.lock"), second.join("Cargo.lock")).unwrap();
    success(p.command_in(&second).args(["check"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"7 one:none\n");
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    // Where nothing was checked first, the build is put back with what it
    // had taken from the check: it was recorded as resting on it.
    let third = p.temp.path().join("third/checkout");
    populate(&third);
    ready(&third);
    fs::copy(p.root.join("Cargo.lock"), third.join("Cargo.lock")).unwrap();
    let out = success(p.command_in(&third).args(["run", "-q"]));
    assert_eq!(out.stdout, b"7 one:none\n");
    assert_eq!(p.count("build_hit"), 3, "{}", p.stats());
    assert_eq!(p.stale_for_cargo(&third, &third.join("target")), 0);
    // And a change there starts from it like from any other build.
    let fourth = p.temp.path().join("fourth/checkout");
    populate(&fourth);
    ready(&fourth);
    fs::copy(p.root.join("Cargo.lock"), fourth.join("Cargo.lock")).unwrap();
    edit_leaf(&fourth, "two");
    make_old(&fourth.join("leaf/src/value.rs"));
    let out = success(p.command_in(&fourth).args(["run", "-q"]));
    assert_eq!(out.stdout, b"7 two\n");
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    // What the check had made of the changed crate was left out, and the
    // build had no use for it. A check there makes it.
    success(p.command_in(&fourth).args(["check"]));
    assert_eq!(p.stale_for_cargo(&fourth, &fourth.join("target")), 0);
}

#[test]
fn a_file_a_build_script_writes_that_names_its_checkout_is_the_same_file_in_another() {
    if !observes() {
        return;
    }
    // The script writes source that names a file by its full path, as
    // generated code does. The crate that includes it is compiled once.
    let p = Project::new();
    let ready = |root: &Path| {
        write(
            root.join("build.rs"),
            r#"fn main() {
    let here = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    let code = format!("const TEXT: &str = include_str!({:?});", format!("{here}/leaf/src/text file"));
    std::fs::write(format!("{out}/made.rs"), code).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
"#,
        );
        write(
            root.join("src/main.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/made.rs\"));\nfn main() { println!(\"{TEXT} {}\", leaf::value()); }",
        );
    };
    ready(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one one:none\n");
    let second = p.temp.path().join("second/checkout");
    populate(&second);
    ready(&second);
    fs::copy(p.root.join("Cargo.lock"), second.join("Cargo.lock")).unwrap();
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&second).args(["run", "-q"]));
        assert_eq!(out.stdout, b"one one:none\n");
    });
    assert_eq!((hits, misses), (3, 0), "{}", p.stats());
    // The file it names is that checkout's own.
    write(second.join("leaf/src/text file"), "two");
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"two two:none\n");
}

#[test]
fn what_a_build_script_reads_of_a_repository_is_an_input() {
    if !observes() || !has_git() {
        return;
    }
    // Cargo's own look at the repository is no input of a build. A build
    // script that puts the branch into the program has read it for itself.
    let p = Project::whole();
    commit_all(&p);
    write(
        p.root.join("build.rs"),
        r#"fn main() {
    let head = std::fs::read_to_string(".git/HEAD").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/head.rs"), format!("const HEAD: &str = {:?};", head.trim())).unwrap();
}
"#,
    );
    write(
        p.root.join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/head.rs\"));\nfn main() { println!(\"{HEAD} {}\", leaf::value()); }",
    );
    git(&p.root, &["commit", "-q", "-a", "-m", "say where"]);
    assert_eq!(
        p.run(&["run", "-q"]).stdout,
        b"ref: refs/heads/main one:none\n"
    );
    git(&p.root, &["checkout", "-q", "-b", "topic"]);
    p.clean();
    assert_eq!(
        p.run(&["run", "-q"]).stdout,
        b"ref: refs/heads/topic one:none\n"
    );
    assert_eq!(p.count("build_hit"), 0, "{}", p.stats());
    git(&p.root, &["checkout", "-q", "main"]);
    p.clean();
    assert_eq!(
        p.run(&["run", "-q"]).stdout,
        b"ref: refs/heads/main one:none\n"
    );
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

/// Give a file a time long before anything was built, as a file copied
/// with its times from somewhere else has. Cargo, which goes by times,
/// then takes whatever was built before for newer than the file.
fn make_old(path: &Path) {
    let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(long_ago).unwrap();
}

/// A checkout of `p`'s sources somewhere else, with the lock file `p`'s
/// build wrote.
fn checkout(p: &Project, name: &str) -> PathBuf {
    let root = p.temp.path().join(name).join("checkout");
    populate(&root);
    add_side(&root);
    fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    root
}

/// A second library the application uses, which nothing in these tests
/// changes.
fn add_side(root: &Path) {
    write(
        root.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nleaf={path='leaf'}\nside={path='side'}\n",
    );
    write(
        root.join("side/Cargo.toml"),
        "[package]\nname='side'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(root.join("side/src/lib.rs"), "pub fn side() -> u32 { 5 }");
    write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{} {}\", leaf::value(), side::side()); }",
    );
}

#[test]
fn a_build_of_changed_sources_starts_from_one_made_before() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    add_side(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none 5\n");
    // Another checkout, with one library changed before anything is built
    // there, and changed in a file that looks older than the first build.
    // The first build's files are put in place without what the library's
    // compiler wrote. Cargo finds that missing and compiles the library and
    // the program that uses it, and nothing else: the other library's
    // compiler is not even asked for.
    let second = checkout(&p, "second");
    edit_leaf(&second, "two");
    make_old(&second.join("leaf/src/value.rs"));
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&second).args(["run", "-q"]));
        assert_eq!(out.stdout, b"two 5\n");
    });
    assert_eq!((hits, misses), (0, 2), "{}", p.stats());
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    assert_eq!(p.count("build_hit"), 0, "{}", p.stats());
    // That build is recorded like one made from nothing, and the next
    // checkout with the same change is given it whole.
    let third = checkout(&p, "third");
    edit_leaf(&third, "two");
    let out = success(p.command_in(&third).args(["run", "-q"]));
    assert_eq!(out.stdout, b"two 5\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // Cargo agrees that everything there is up to date.
    assert_eq!(p.stale_for_cargo(&third, &third.join("target")), 0);
    // A checkout of the first sources still gets the first build, and one
    // with yet another change starts from whichever is nearer.
    let fourth = checkout(&p, "fourth");
    let out = success(p.command_in(&fourth).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none 5\n");
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    let fifth = checkout(&p, "fifth");
    edit_leaf(&fifth, "three");
    write(fifth.join("side/src/lib.rs"), "pub fn side() -> u32 { 6 }");
    let out = success(p.command_in(&fifth).args(["run", "-q"]));
    assert_eq!(out.stdout, b"three 6\n");
    assert_eq!(p.count("build_near"), 2, "{}", p.stats());
    // Without the old build to start from, the same sources give the
    // same program.
    let sixth = checkout(&p, "sixth");
    edit_leaf(&sixth, "four");
    let out = success(
        p.command_in(&sixth)
            .env("JATO_NEAR", "0")
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, b"four 5\n");
    assert_eq!(p.count("build_near"), 2, "{}", p.stats());
}

#[test]
fn a_build_started_from_another_and_then_doubted_is_made_from_nothing() {
    if !observes() {
        return;
    }
    // Should Cargo ever call a build done with a file missing that was
    // left out for it to make again, everything put in place is taken
    // away and Cargo builds from nothing. Nothing makes Cargo do that, so
    // the test says it happened.
    let p = Project::whole();
    add_side(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none 5\n");
    let second = checkout(&p, "second");
    edit_leaf(&second, "two");
    let (hits, misses) = p.during(|| {
        let out = success(
            p.command_in(&second)
                .env("JATO_TEST_DOUBT", "a file")
                .args(["run", "-q"]),
        );
        assert_eq!(out.stdout, b"two 5\n");
    });
    assert_eq!(p.count("build_doubted"), 1, "{}", p.stats());
    // The library and the program were compiled when the build started
    // from the first one. From nothing, they and the other library are
    // found in the cache.
    assert_eq!((hits, misses), (3, 2), "{}", p.stats());
    // And that build is recorded.
    let third = checkout(&p, "third");
    edit_leaf(&third, "two");
    let out = success(p.command_in(&third).args(["run", "-q"]));
    assert_eq!(out.stdout, b"two 5\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

#[test]
fn what_only_a_macro_reads_is_compiled_again_in_a_build_started_from_another() {
    if !observes() {
        return;
    }
    // Cargo knows nothing of a file a procedural macro reads. The crate
    // that used the macro is compiled again all the same, because what
    // its compiler wrote is left out of the files put in place.
    let p = Project::whole();
    let ready = |root: &Path, hidden: &str| {
        add_macros(root);
        write(
            root.join("leaf/Cargo.toml"),
            "[package]\nname='leaf'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmacros={path='../macros'}\n",
        );
        write(root.join("leaf/src/lib.rs"), "pub use macros::*;");
        write(
            root.join("src/main.rs"),
            "fn main() { println!(\"{}\", leaf::hidden!()); }",
        );
        write(root.join("hidden.txt"), hidden);
    };
    ready(&p.root, "first");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"first\n");
    let second = p.temp.path().join("second/checkout");
    populate(&second);
    ready(&second, "second");
    make_old(&second.join("hidden.txt"));
    fs::copy(p.root.join("Cargo.lock"), second.join("Cargo.lock")).unwrap();
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"second\n");
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
}

#[test]
fn what_a_build_script_reads_has_it_run_again_and_settings_leave_nothing_to_start_from() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // A build script that reads a file without telling Cargo, and a
    // setting of Cargo's.
    let ready = |root: &Path, number: &str| {
        add_side(root);
        write(root.join("number.txt"), number);
        write(
            root.join("build.rs"),
            r#"fn main() {
    let number = std::fs::read_to_string("number.txt").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/number.rs"), format!("const NUMBER: u32 = {};", number.trim())).unwrap();
}
"#,
        );
        write(
            root.join("src/main.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/number.rs\"));\nfn main() { println!(\"{NUMBER} {} {}\", leaf::value(), side::side()); }",
        );
    };
    ready(&p.root, "1");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"1 one:none 5\n");
    let copy = |name: &str, number: &str| {
        let root = checkout(&p, name);
        ready(&root, number);
        root
    };
    // What the build script read differs: Cargo would not run it again
    // in a target directory that holds its output. It runs again all the
    // same, for the compiler that made it does.
    let second = copy("second", "2");
    make_old(&second.join("number.txt"));
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"2 one:none 5\n");
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    assert_eq!(p.stale_for_cargo(&second, &second.join("target")), 0);
    // A manifest differs, and the lock file that went with it is gone.
    let third = copy("third", "1");
    let manifest = fs::read_to_string(third.join("side/Cargo.toml")).unwrap();
    write(
        third.join("side/Cargo.toml"),
        &manifest.replace("0.1.0", "0.1.1"),
    );
    fs::remove_file(third.join("Cargo.lock")).unwrap();
    let out = success(p.command_in(&third).args(["run", "-q"]));
    assert_eq!(out.stdout, b"1 one:none 5\n");
    // That is Cargo's own to plan the build with again, and the build
    // starts from the first: see the test of manifests that differ.
    assert_eq!(p.count("build_near"), 2, "{}", p.stats());
    // A file appears where Cargo looked for programs to build and found
    // no directory: Cargo's to find, like the manifest.
    let fourth = copy("fourth", "1");
    write(fourth.join("src/bin/extra.rs"), "fn main() {}");
    success(p.command_in(&fourth).args(["build"]));
    assert!(fourth.join("target/debug/extra").exists());
    assert_eq!(p.count("build_near"), 3, "{}", p.stats());
    assert_eq!(p.stale_for_cargo(&fourth, &fourth.join("target")), 0);
    // Settings of Cargo's appear. What they do to a compilation Cargo
    // does not always remember, so the build is carried out.
    let sixth = copy("sixth", "1");
    write(
        sixth.join(".cargo/config.toml"),
        "[env]\nLEAF_ENV = \"set\"\n",
    );
    let out = success(p.command_in(&sixth).args(["run", "-q"]));
    assert_eq!(out.stdout, b"1 one:set 5\n");
    assert_eq!(p.count("build_near"), 3, "{}", p.stats());
    // The build script itself differs. Its compiler is one of Cargo's, so
    // the build starts from the first, leaves out the earlier script's
    // outputs so the new version runs in a clean directory, and records
    // the finished build for other checkouts.
    let fifth = copy("fifth", "1");
    let script = fs::read_to_string(fifth.join("build.rs")).unwrap();
    write(
        fifth.join("build.rs"),
        &script.replace("number.trim()", "number.trim().repeat(2)"),
    );
    make_old(&fifth.join("build.rs"));
    let stored = p.count("build_stored");
    let out = success(p.command_in(&fifth).args(["run", "-q"]));
    assert_eq!(out.stdout, b"11 one:none 5\n");
    assert_eq!(p.count("build_near"), 4, "{}", p.stats());
    assert_eq!(p.count("build_stored"), stored + 1, "{}", p.stats());
    // A copy of `fifth` gets that recorded build whole.
    let seventh = checkout(&p, "seventh");
    ready(&seventh, "1");
    write(
        seventh.join("build.rs"),
        &script.replace("number.trim()", "number.trim().repeat(2)"),
    );
    let hits = p.count("build_hit");
    let out = success(p.command_in(&seventh).args(["run", "-q"]));
    assert_eq!(out.stdout, b"11 one:none 5\n");
    assert_eq!(p.count("build_hit"), hits + 1, "{}", p.stats());
}

/// Every file under a target directory but the compiler's saved state,
/// whose directories are named anew each time, and Jato's own note of
/// the builds it recorded there.
fn files_of(target: &Path) -> std::collections::BTreeSet<PathBuf> {
    fn walk(dir: &Path, top: &Path, into: &mut std::collections::BTreeSet<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().map(|entry| entry.unwrap()) {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                if entry.file_name() != "incremental" {
                    walk(&path, top, into);
                }
            } else if !entry.file_name().to_string_lossy().starts_with(".jato-") {
                into.insert(path.strip_prefix(top).unwrap().to_owned());
            }
        }
    }
    let mut all = std::collections::BTreeSet::new();
    walk(target, target, &mut all);
    all
}

/// Cargo reads its manifests and lock file whenever it runs and plans the
/// build by them. A build whose manifests differ from a recorded one's
/// starts from that one, and Cargo is left to see what follows: which
/// compilations are as they were, which have to be run, and which are no
/// longer part of the build.
#[test]
fn a_build_whose_manifests_differ_starts_from_one_made_before() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    add_side(&p.root);
    write(p.root.join("src/bin/one.rs"), "fn main() {}");
    assert_eq!(
        p.run(&["run", "-q", "--bin", "app"]).stdout,
        b"one:none 5\n"
    );
    p.run(&["build"]);
    let program = |root: &Path| {
        let out = Command::new(root.join("target/debug/app"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let copy = |name: &str| {
        let root = checkout(&p, name);
        write(root.join("src/bin/one.rs"), "fn main() {}");
        root
    };
    // What a build of the same sources from nothing leaves in its target
    // directory, with a cache of its own.
    let from_nothing = |root: &Path| {
        let target = root.join("target-from-nothing");
        success(
            p.command_in(root)
                .env("CARGO_TARGET_DIR", &target)
                .env("JATO_CACHE_DIR", root.join("cache-of-its-own"))
                .args(["build"]),
        );
        files_of(&target)
    };
    let build = |root: &Path| {
        let out = success(p.command_in(root).env("JATO_LOG", "debug").args(["build"]));
        let said = decisions(&out, "cargo");
        assert!(
            said.iter()
                .any(|d| d.contains("Cargo plans the build again")),
            "{said:?}"
        );
        out
    };
    let compiled =
        |out: &Output, name: &str| decisions(out, name).iter().any(|d| d.starts_with("stored"));

    // A manifest that says something more, of no consequence to any
    // compilation: nothing is compiled.
    let second = copy("second");
    let manifest = fs::read_to_string(second.join("Cargo.toml")).unwrap();
    write(
        second.join("Cargo.toml"),
        &format!("{manifest}\n[package.metadata.notes]\nkept = true\n"),
    );
    let (hits, misses) = p.during(|| drop(build(&second)));
    assert_eq!((hits, misses), (0, 0), "{}", p.stats());
    assert_eq!(p.count("build_replanned"), 1, "{}", p.stats());
    assert_eq!(program(&second), "one:none 5\n");
    assert_eq!(p.stale_for_cargo(&second, &second.join("target")), 0);
    assert_eq!(files_of(&second.join("target")), from_nothing(&second));
    // And the build is one to put back.
    let stored = p.count("build_hit");
    fs::remove_dir_all(second.join("target")).unwrap();
    success(p.command_in(&second).args(["build"]));
    assert_eq!(p.count("build_hit"), stored + 1, "{}", p.stats());
    assert_eq!(p.stale_for_cargo(&second, &second.join("target")), 0);

    // A library's edition changes: the library is compiled, and what
    // uses it. The third crate is not.
    let third = copy("third");
    let manifest = fs::read_to_string(third.join("leaf/Cargo.toml")).unwrap();
    write(
        third.join("leaf/Cargo.toml"),
        &manifest.replace("2021", "2018"),
    );
    let out = build(&third);
    assert!(compiled(&out, "leaf") && compiled(&out, "app"), "{:?}", out);
    assert!(
        decisions(&out, "side").is_empty(),
        "{:?}",
        decisions(&out, "side")
    );
    assert_eq!(program(&third), "one:none 5\n");
    assert_eq!(p.stale_for_cargo(&third, &third.join("target")), 0);
    // What the compilers of the library as it was had written is gone,
    // with what Cargo kept of them: file for file what a build from
    // nothing leaves.
    assert_eq!(files_of(&third.join("target")), from_nothing(&third));

    // A library gets a new version: Cargo compiles the library and what
    // uses it, and writes the lock file. A build that changed what it
    // had read is not recorded, as ever.
    let fourth = copy("fourth");
    let manifest = fs::read_to_string(fourth.join("side/Cargo.toml")).unwrap();
    write(
        fourth.join("side/Cargo.toml"),
        &manifest.replace("0.1.0", "0.2.0"),
    );
    let lock = fs::read_to_string(fourth.join("Cargo.lock")).unwrap();
    let out = build(&fourth);
    assert!(compiled(&out, "side") && compiled(&out, "app"), "{:?}", out);
    assert!(
        decisions(&out, "leaf").is_empty(),
        "{:?}",
        decisions(&out, "leaf")
    );
    assert_ne!(fs::read_to_string(fourth.join("Cargo.lock")).unwrap(), lock);
    assert_eq!(program(&fourth), "one:none 5\n");
    assert_eq!(p.stale_for_cargo(&fourth, &fourth.join("target")), 0);
    assert_eq!(files_of(&fourth.join("target")), from_nothing(&fourth));

    // A program is added where Cargo looks for programs, and a crate
    // beside the others that the application now uses.
    let fifth = copy("fifth");
    write(
        fifth.join("src/bin/two.rs"),
        "fn main() { println!(\"two\"); }",
    );
    write(
        fifth.join("more/Cargo.toml"),
        "[package]\nname='more'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(fifth.join("more/src/lib.rs"), "pub fn more() -> u32 { 9 }");
    let manifest = fs::read_to_string(fifth.join("Cargo.toml")).unwrap();
    write(
        fifth.join("Cargo.toml"),
        &format!("{manifest}more={{path='more'}}\n"),
    );
    write(
        fifth.join("src/main.rs"),
        "fn main() { println!(\"{} {} {}\", leaf::value(), side::side(), more::more()); }",
    );
    let out = build(&fifth);
    assert!(compiled(&out, "more") && compiled(&out, "two"), "{:?}", out);
    assert!(decisions(&out, "leaf").is_empty() && decisions(&out, "side").is_empty());
    assert_eq!(program(&fifth), "one:none 5 9\n");
    let two = Command::new(fifth.join("target/debug/two"))
        .output()
        .unwrap();
    assert_eq!(two.stdout, b"two\n");
    assert_eq!(p.stale_for_cargo(&fifth, &fifth.join("target")), 0);
    assert_eq!(files_of(&fifth.join("target")), from_nothing(&fifth));
    assert_eq!(p.count("build_replanned"), 4, "{}", p.stats());

    // A manifest that a build script reads as well is that script's to
    // read again: it runs, and Cargo plans the build.
    let q = Project::whole();
    let script = "fn main() { let m = std::fs::read_to_string(\"Cargo.toml\").unwrap(); println!(\"cargo:rustc-env=LEAF_ENV={}\", m.len()); println!(\"cargo:rerun-if-changed=build.rs\"); }";
    write(q.root.join("leaf/build.rs"), script);
    q.run(&["build"]);
    let sixth = q.temp.path().join("sixth");
    populate(&sixth);
    write(sixth.join("leaf/build.rs"), script);
    fs::copy(q.root.join("Cargo.lock"), sixth.join("Cargo.lock")).unwrap();
    let manifest = fs::read_to_string(sixth.join("leaf/Cargo.toml")).unwrap();
    write(
        sixth.join("leaf/Cargo.toml"),
        &format!("{manifest}\n[package.metadata.notes]\nkept = true\n"),
    );
    success(q.command_in(&sixth).args(["build"]));
    assert_eq!(q.count("build_near"), 1, "{}", q.stats());
    assert_eq!(q.count("build_replanned"), 1, "{}", q.stats());

    // A library with a build script gets a new version. Cargo runs the
    // script for the library as it now is, in a directory of its own,
    // and what the script wrote for the library as it was goes with what
    // Cargo kept of that run.
    let r = Project::whole();
    let script = "fn main() { let out = std::env::var(\"OUT_DIR\").unwrap(); std::fs::write(format!(\"{out}/made.rs\"), \"pub const MADE: u32 = 3;\").unwrap(); println!(\"cargo:rerun-if-changed=build.rs\"); }";
    let ready = |root: &Path| {
        write(root.join("leaf/build.rs"), script);
        write(
            root.join("leaf/src/lib.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/made.rs\"));\npub fn value() -> u32 { MADE }",
        );
    };
    ready(&r.root);
    assert_eq!(r.run(&["run", "-q"]).stdout, b"3\n");
    r.run(&["build"]);
    let seventh = r.temp.path().join("seventh");
    populate(&seventh);
    ready(&seventh);
    let manifest = fs::read_to_string(seventh.join("leaf/Cargo.toml")).unwrap();
    write(
        seventh.join("leaf/Cargo.toml"),
        &manifest.replace("0.1.0", "0.3.0"),
    );
    success(r.command_in(&seventh).args(["build"]));
    assert_eq!(r.count("build_replanned"), 1, "{}", r.stats());
    let out = Command::new(seventh.join("target/debug/app"))
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"3\n");
    assert_eq!(r.stale_for_cargo(&seventh, &seventh.join("target")), 0);
    let target = seventh.join("target-from-nothing");
    success(
        r.command_in(&seventh)
            .env("CARGO_TARGET_DIR", &target)
            .env("JATO_CACHE_DIR", seventh.join("cache-of-its-own"))
            .args(["build"]),
    );
    assert_eq!(files_of(&seventh.join("target")), files_of(&target));
    let out = Command::new(sixth.join("target/debug/app"))
        .output()
        .unwrap();
    assert_eq!(
        out.stdout,
        format!("one:{}\n", manifest.len() + 38).as_bytes()
    );
}

#[test]
fn a_build_started_from_another_that_fails_is_finished_by_the_next() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    add_side(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none 5\n");
    let second = checkout(&p, "second");
    let value = second.join("leaf/src/value.rs");
    write(&value, "pub fn value() -> String { broken }");
    make_old(&value);
    let failed = p.command_in(&second).args(["run", "-q"]).output().unwrap();
    assert!(!failed.status.success());
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    // Mended, with a time as old as before.
    edit_leaf(&second, "mended");
    make_old(&value);
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"mended 5\n");
    // And back to what the first build was made from.
    populate(&second);
    add_side(&second);
    make_old(&value);
    let out = success(p.command_in(&second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none 5\n");
}

#[test]
fn changed_c_that_a_build_script_compiles_has_the_script_run_again() {
    if !observes() {
        return;
    }
    // Cargo does not know what a build script's compiler read, and would
    // not run the script again for a header that looks older than what
    // the script wrote. The record of the build says which script it was
    // that had the header read, and the script runs again because the
    // compiler that made it does.
    let p = Project::whole();
    compile_c(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"42\n");
    let copy = |name: &str| {
        let root = p.temp.path().join(name).join("checkout");
        populate(&root);
        compile_c(&root);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
        write(root.join("native/answer.h"), "#define ANSWER 44\n");
        make_old(&root.join("native/answer.h"));
        root
    };
    let second = copy("second");
    let compiled = p.count("tool_stored");
    let out = success(
        p.command_in(&second)
            .env("JATO_LOG", "debug")
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, b"44\n");
    let said = decisions(&out, "cargo");
    assert!(
        said.iter()
            .any(|d| d.contains("a build script to run again")),
        "{said:?}"
    );
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_stored"), compiled + 1, "{}", p.stats());
    // The library has nothing to do with it.
    assert!(
        decisions(&out, "leaf").is_empty(),
        "{:?}",
        decisions(&out, "leaf")
    );
    assert_eq!(p.stale_for_cargo(&second, &second.join("target")), 0);
    // The build is one to put back.
    let third = copy("third");
    let whole = p.count("build_hit");
    assert_eq!(
        success(p.command_in(&third).args(["run", "-q"])).stdout,
        b"44\n"
    );
    assert_eq!(p.count("build_hit"), whole + 1, "{}", p.stats());
}

/// Run a command whose input and output are a terminal of so many
/// columns. Returns whether it succeeded and everything it wrote there.
fn on_terminal(cmd: &mut Command, columns: u16) -> (bool, Vec<u8>) {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    size.ws_col = columns;
    size.ws_row = 40;
    let (mut ours, mut theirs) = (0, 0);
    let made = unsafe {
        libc::openpty(
            &raw mut ours,
            &raw mut theirs,
            std::ptr::null_mut(),
            std::ptr::null(),
            &raw const size,
        )
    };
    assert_eq!(made, 0);
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(ours), OwnedFd::from_raw_fd(theirs)) };
    let mut child = cmd
        .stdin(theirs.try_clone().unwrap())
        .stdout(theirs.try_clone().unwrap())
        .stderr(theirs)
        .spawn()
        .unwrap();
    // The command keeps its ends until it is told of others, and the
    // terminal would never be seen to close.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut written = Vec::new();
    let mut terminal = fs::File::from(ours);
    let mut buffer = [0u8; 4096];
    // Reading fails once nobody holds the other end.
    while let Ok(n) = terminal.read(&mut buffer) {
        if n == 0 {
            break;
        }
        written.extend_from_slice(&buffer[..n]);
    }
    (child.wait().unwrap().success(), written)
}

#[test]
fn a_build_is_the_same_build_on_any_terminal_and_on_none() {
    if !observes() {
        return;
    }
    const FINISHED: &[u8] = b"\x1b[1m\x1b[92m    Finished\x1b[0m";
    let within = |text: &[u8], part: &[u8]| text.windows(part.len()).any(|at| at == part);
    let p = Project::whole();
    // Made with nobody watching, as an agent makes it.
    p.run(&["build"]);
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // Put back for someone at a terminal, in the terminal's colours, at
    // whatever width.
    for (round, columns) in [(1, 100), (2, 60)] {
        p.clean();
        let (worked, shown) = on_terminal(p.command().args(["build"]), columns);
        assert!(worked, "{}", String::from_utf8_lossy(&shown));
        assert_eq!(p.count("build_hit"), round, "{}", p.stats());
        assert!(
            within(&shown, FINISHED),
            "{}",
            String::from_utf8_lossy(&shown)
        );
    }
    // A build with a warning, made on a terminal: the compiler's warning
    // is in colour there.
    write(
        p.root.join("leaf/src/value.rs"),
        "pub fn value() -> String { let unused = 1; String::from(\"three\") }",
    );
    p.clean();
    let (worked, shown) = on_terminal(p.command().args(["build"]), 100);
    assert!(worked && within(&shown, b"unused"));
    assert!(within(&shown, b"\x1b[33mwarning") || within(&shown, b"mwarning"));
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
    // Put back in a narrower window as it was laid out, which is what
    // Cargo does with the warnings of a crate it finds up to date.
    p.clean();
    let (worked, shown) = on_terminal(p.command().args(["build"]), 60);
    assert!(worked && within(&shown, b"unused") && within(&shown, FINISHED));
    assert_eq!(p.count("build_hit"), 3, "{}", p.stats());
    // And where there is no terminal, without the colours.
    p.clean();
    let out = p.run(&["build"]);
    assert_eq!(p.count("build_hit"), 4, "{}", p.stats());
    assert!(
        !out.stderr.contains(&0x1b),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(within(&out.stderr, b"warning: unused variable: `unused`"));
    assert!(within(&out.stderr, b"    Finished"));
    // The other way round: recorded without a terminal, with the warning.
    write(
        p.root.join("leaf/src/value.rs"),
        "pub fn value() -> String { let idle = 1; String::from(\"four\") }",
    );
    p.clean();
    let out = p.run(&["build"]);
    assert!(within(&out.stderr, b"warning: unused variable: `idle`"));
    p.clean();
    let (worked, shown) = on_terminal(p.command().args(["build"]), 100);
    assert!(worked && within(&shown, b"unused variable: `idle`") && within(&shown, FINISHED));
    assert_eq!(p.count("build_hit"), 5, "{}", p.stats());
}

/// A build is right for the sources as they were when Jato looked at
/// them. One edited after that, while the files are still being written,
/// has to look newer to Cargo than the build does.
#[test]
fn an_edit_made_while_a_build_is_put_back_is_newer_than_the_build() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    p.run(&["build"]);
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    for target in [true, false] {
        // Once into a target directory that is there, and once where
        // there is none.
        fs::remove_dir_all(p.root.join("target")).unwrap();
        if target {
            fs::create_dir(p.root.join("target")).unwrap();
        }
        let hold = p.temp.path().join(format!("go-{target}"));
        let mut build = p
            .command()
            .env("JATO_TEST_HOLD", &hold)
            .args(["build"])
            .spawn()
            .unwrap();
        let waiting = hold.with_extension("waiting");
        for _ in 0..4000 {
            if waiting.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(waiting.exists(), "the build never looked at its sources");
        // Saved again as it was: the time is what Cargo goes by.
        write(p.root.join("leaf/src/text file"), "one");
        write(&hold, "");
        assert!(build.wait().unwrap().success());
        assert_eq!(p.stale_for_cargo(&p.root, &p.root.join("target")), 2);
    }
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
}

#[test]
fn an_edit_right_after_a_build_is_put_back_is_compiled() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    p.run(&["build"]);
    // A file written in the same instant as the files of a build that was
    // put back must still look newer to Cargo than they do. The kernel
    // gives a written file the time of its last clock tick, which is a
    // little behind the clock a program reads.
    for round in 0..12 {
        p.clean();
        p.run(&["build"]);
        assert_eq!(p.count("build_hit"), round + 1, "{}", p.stats());
        let text = format!("round {round}");
        edit_leaf(&p.root, &text);
        let out = success(
            p.command()
                .env_remove("JATO_EDIT_WINDOW")
                .args(["run", "-q"]),
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{text}\n"));
        // Back to the build that is recorded.
        write(
            p.root.join("leaf/src/value.rs"),
            "pub fn value() -> String { format!(\"{}:{}\", include_str!(\"text file\"), option_env!(\"LEAF_ENV\").unwrap_or(\"none\")) }",
        );
    }
}

#[test]
fn a_build_recorded_once_is_filled_in_without_running_cargo() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    write(
        p.root.join("src/main.rs"),
        "fn main() { let unused_marker = 1; println!(\"{}\", leaf::value()); }",
    );
    let text = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    let first = text(&p.run(&["build"]));
    assert!(
        first.contains("Compiling leaf") && first.contains("unused_marker"),
        "{first}"
    );
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    let original = fs::read(p.root.join("target/debug/app")).unwrap();

    // Nothing changed: Cargo is not started, and what it would have said
    // about a build with nothing to do is said for it.
    let again = text(&p.run(&["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert!(!again.contains("Compiling"), "{again}");
    assert!(
        again.contains("unused_marker") && again.contains("Finished"),
        "{again}"
    );
    assert!(again.contains("generated 1 warning"), "{again}");

    // The whole target directory comes back, and Cargo finds nothing in
    // it that it would build again.
    p.clean();
    let (hits, misses) = p.during(|| drop(p.run(&["build"])));
    assert_eq!((hits, misses), (0, 0), "{}", p.stats());
    assert_eq!(p.count("build_hit"), 2);
    assert_eq!(fs::read(p.root.join("target/debug/app")).unwrap(), original);
    assert_eq!(p.stale_for_cargo(&p.root, &p.root.join("target")), 0);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");

    // Another target directory, and the same sources somewhere else.
    let elsewhere = p.temp.path().join("other/target");
    let before = p.count("build_hit");
    success(
        p.command()
            .env("CARGO_TARGET_DIR", &elsewhere)
            .args(["build"]),
    );
    assert_eq!(p.count("build_hit"), before + 1, "{}", p.stats());
    assert_eq!(p.stale_for_cargo(&p.root, &elsewhere), 0);
    let copy = p.temp.path().join("second/checkout");
    populate(&copy);
    fs::copy(p.root.join("src/main.rs"), copy.join("src/main.rs")).unwrap();
    fs::copy(p.root.join("Cargo.lock"), copy.join("Cargo.lock")).unwrap();
    success(p.command_in(&copy).args(["build"]));
    assert_eq!(p.count("build_hit"), before + 2, "{}", p.stats());
    assert_eq!(p.count("build_miss"), 1, "{}", p.stats());
    assert_eq!(fs::read(copy.join("target/debug/app")).unwrap(), original);
    assert_eq!(p.stale_for_cargo(&copy, &copy.join("target")), 0);
    assert_eq!(
        success(p.command_in(&copy).args(["run", "-q"])).stdout,
        b"one:none\n"
    );
}

/// A pipe with a name, holding so many tokens, and its name.
fn tokens(dir: &Path, count: usize) -> (fs::File, PathBuf) {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("tokens");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut pipe = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .unwrap();
    pipe.write_all(&vec![b'|'; count]).unwrap();
    (pipe, path)
}

/// A library of several modules, which the compiler turns into machine
/// code on as many threads as it gets tokens for.
fn several_modules(root: &Path) {
    let mut lib = String::from("mod value; pub use value::value;\n");
    for name in ["a", "b", "c", "d", "e", "f"] {
        lib.push_str(&format!("pub mod {name};\n"));
        write(
            root.join(format!("leaf/src/{name}.rs")),
            &format!(
                "pub fn {name}(n: u64) -> String {{ (0..n).map(|i| format!(\"{name}{{i}}\")).collect() }}\n"
            ),
        );
    }
    write(root.join("leaf/src/lib.rs"), &lib);
}

/// Whoever starts a build may share its tokens with it, make for one, in
/// a pipe that has a name. Every compiler opens that file, takes tokens
/// out of it and puts them back.
#[test]
fn tokens_in_a_pipe_with_a_name_are_no_input_of_a_compiler() {
    if !observes() {
        return;
    }
    let p = Project::new();
    several_modules(&p.root);
    let (_pipe, path) = tokens(&p.temp.path().join("shared"), 3);
    let told = format!("-j4 --jobserver-auth=fifo:{}", path.display());
    let build = || {
        success(
            p.command()
                .env("CARGO_MAKEFLAGS", &told)
                .args(["build", "--release"]),
        )
    };
    let (hits, misses) = p.during(|| drop(build()));
    assert_eq!((hits, misses), (0, 2), "{}", p.stats());
    assert_eq!(p.count("uncacheable"), 0, "{}", p.stats());
    p.clean();
    let (hits, misses) = p.during(|| drop(build()));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}

/// Cargo makes a pipe for its tokens unless it is given one, and to hand
/// on a pipe without a name it starts every compiler as a copy of itself.
/// Jato gives it one with a name, and only where nothing says how many
/// compilers to run.
#[test]
fn cargo_is_given_its_tokens_by_name_unless_something_says_how_many() {
    if !observes() || std::thread::available_parallelism().is_ok_and(|n| n.get() < 2) {
        return;
    }
    let p = Project::whole();
    several_modules(&p.root);
    write(
        p.root.join("leaf/build.rs"),
        r#"fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let told = std::env::var("CARGO_MAKEFLAGS").unwrap_or_default();
    let how = if told.contains("--jobserver-auth=fifo:") { "by name" } else { "handed down" };
    println!("cargo:rustc-env=LEAF_ENV={how}");
}
"#,
    );
    let program = |target: &Path| {
        let out = Command::new(target.join("debug/app")).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let build = |target: &Path, more: &[&str]| {
        success(
            p.command()
                .env("JATO_JOBSERVER", "1")
                .env("CARGO_TARGET_DIR", target)
                .arg("build")
                .args(more),
        )
    };
    let first = p.root.join("target");
    build(&first, &[]);
    assert_eq!(program(&first), "one:by name\n");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // The pipe went with the build.
    let left: Vec<_> = fs::read_dir(p.temp.path().join("cache/locks/jobs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("tokens-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // A build told how many is Cargo's to arrange, on the command line
    // or in its settings.
    let second = p.temp.path().join("second");
    build(&second, &["-j2"]);
    assert_eq!(program(&second), "one:handed down\n");
    write(p.root.join(".cargo/config.toml"), "[build]\njobs = 2\n");
    let third = p.temp.path().join("third");
    build(&third, &[]);
    assert_eq!(program(&third), "one:handed down\n");
}

/// A build has more files than a process may hold open at once.
#[test]
fn a_build_of_more_files_than_may_be_open_at_once_is_put_back() {
    use std::os::unix::process::CommandExt;
    if !observes() {
        return;
    }
    let p = Project::whole();
    for name in ["a", "b", "c", "d", "e", "f"] {
        write(
            p.root.join(format!("src/bin/{name}.rs")),
            "fn main() { println!(\"{}\", leaf::value()); }",
        );
    }
    p.run(&["build"]);
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    fn files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .map(|entry| match entry.file_type().unwrap().is_dir() {
                true => files(&entry.path()),
                false => 1,
            })
            .sum()
    }
    p.clean();
    let mut cmd = p.command();
    cmd.args(["build"]);
    unsafe {
        cmd.pre_exec(|| {
            let mut limit: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = 40;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    success(&mut cmd);
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    let back = files(&p.root.join("target"));
    assert!(back > 64, "only {back} files were put back");
    assert_eq!(p.stale_for_cargo(&p.root, &p.root.join("target")), 0);
}

#[test]
fn a_recorded_build_gives_way_to_anything_that_changed() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    p.run(&["build"]);
    // Builds restored whole, and builds that Cargo had to carry out: under
    // watch from an empty target, or on its own in one already built.
    let builds = || {
        (
            p.count("build_hit"),
            p.count("build_miss") + p.count("build_left_to_cargo"),
        )
    };
    assert_eq!(builds(), (0, 1));
    p.run(&["build"]);
    assert_eq!(builds(), (1, 1));

    // An edit, and its undoing.
    write(p.root.join("leaf/src/text file"), "two");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"two:none\n");
    p.run(&["build"]);
    write(p.root.join("leaf/src/text file"), "one");
    p.run(&["build"]);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");

    // A variable the code reads at compile time.
    let before = builds();
    success(p.command().env("LEAF_ENV", "set").args(["build"]));
    assert_eq!(builds().1, before.1 + 1, "{}", p.stats());
    assert_eq!(
        success(p.command().env("LEAF_ENV", "set").args(["run", "-q"])).stdout,
        b"one:set\n"
    );
    // One it does not read.
    let before = builds();
    p.run(&["build"]);
    success(p.command().env("UNRELATED", "x").args(["build"]));
    assert_eq!(builds(), (before.0 + 2, before.1), "{}", p.stats());

    // A manifest.
    let manifest = fs::read_to_string(p.root.join("leaf/Cargo.toml")).unwrap();
    write(
        p.root.join("leaf/Cargo.toml"),
        &format!("{manifest}\n[features]\nextra=[]\n"),
    );
    let before = builds();
    p.run(&["build"]);
    assert_eq!(builds().1, before.1 + 1, "{}", p.stats());

    // A file that makes the build fail must fail it, and a failed build
    // is not something to restore. Cargo itself does not look for this
    // file, so the target is cleared to make it run the compiler.
    write(p.root.join("leaf/src/value/mod.rs"), "pub fn value() {}");
    p.clean();
    for _ in 0..2 {
        let fail = p.command().arg("build").output().unwrap();
        assert!(!fail.status.success());
        assert!(String::from_utf8_lossy(&fail.stderr).contains("E0761"));
    }
    fs::remove_dir_all(p.root.join("leaf/src/value")).unwrap();
    p.run(&["build"]);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n");

    // Without whole builds, every command goes to Cargo.
    let before = builds();
    success(p.command().env("JATO_WHOLE_BUILDS", "0").args(["build"]));
    assert_eq!(builds(), before);
}

#[test]
fn a_package_cargo_walks_is_recorded_with_its_target_inside_it() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // A build script that names no inputs: Cargo then looks at every file
    // of the package, by listing the directory the target is made in.
    write(p.root.join("build.rs"), "fn main() {}");
    write(p.root.join("notes/idea.txt"), "first");
    p.run(&["build"]);
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // The target directory now stands in that listing. The build is the
    // same build with it, without it, and in a checkout that has none yet.
    p.run(&["build"]);
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    p.clean();
    p.run(&["build"]);
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    let copy = p.temp.path().join("second/checkout");
    populate(&copy);
    for file in ["build.rs", "notes/idea.txt", "Cargo.lock"] {
        write(
            copy.join(file),
            &fs::read_to_string(p.root.join(file)).unwrap(),
        );
    }
    success(p.command_in(&copy).args(["build"]));
    assert_eq!(p.count("build_hit"), 3, "{}", p.stats());
    assert_eq!(p.count("build_miss"), 1, "{}", p.stats());
    assert_eq!(
        success(p.command_in(&copy).args(["run", "-q"])).stdout,
        b"one:none\n"
    );
    // A file added to the package is nothing this build examined. Cargo
    // listed the directories to compare the files' times with what it had
    // built, and would run the build script again to be safe. The script
    // was watched, and looked at none of this.
    for added in ["notes/another.txt", "extra.txt"] {
        let before = p.count("build_hit");
        write(p.root.join(added), "more");
        p.run(&["build"]);
        assert_eq!(p.count("build_hit"), before + 1, "{added}: {}", p.stats());
    }
    // A build script that looks at what a directory holds has examined
    // it, and a file added there makes another build.
    write(
        p.root.join("build.rs"),
        r#"fn main() {
    let count = std::fs::read_dir("notes").unwrap().count();
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/count.rs"), format!("const COUNT: usize = {count};")).unwrap();
}
"#,
    );
    write(
        p.root.join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/count.rs\"));\nfn main() { println!(\"{COUNT}\"); }",
    );
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"2\n");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"2\n");
    let before = p.count("build_hit");
    write(p.root.join("notes/third.txt"), "more");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"3\n");
    assert_eq!(p.count("build_hit"), before, "{}", p.stats());
}

#[test]
fn a_run_of_edits_is_left_to_rustc_and_a_pause_ends_it() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // As a person would have it: results of edits made minutes apart.
    let spell = || {
        let mut cmd = p.command();
        cmd.env_remove("JATO_EDIT_WINDOW");
        cmd
    };
    let run = |cmd: &mut Command| String::from_utf8(success(cmd.args(["run", "-q"])).stdout);
    assert_eq!(run(&mut spell()).unwrap(), "one:none\n");
    let stored = p.count("stored");
    assert_eq!((stored, p.count("continued")), (2, 0), "{}", p.stats());

    // The first edit after a build is compiled under watch and recorded.
    write(p.root.join("leaf/src/text file"), "two");
    assert_eq!(run(&mut spell()).unwrap(), "two:none\n");
    success(spell().args(["build", "-q"]));
    assert_eq!(p.count("stored"), stored + 2, "{}", p.stats());
    assert_eq!(p.count("continued"), 0, "{}", p.stats());

    // Edits that follow within minutes are compiled by rustc from what it
    // saved, unwatched, and leave nothing in the cache.
    for text in ["three", "four"] {
        write(p.root.join("leaf/src/text file"), text);
        assert_eq!(run(&mut spell()).unwrap(), format!("{text}:none\n"));
        success(spell().args(["build", "-q"]));
    }
    assert_eq!(p.count("stored"), stored + 2, "{}", p.stats());
    assert_eq!(p.count("continued"), 4, "{}", p.stats());

    // The state before them is still known, as a whole build.
    write(p.root.join("leaf/src/text file"), "one");
    let known = p.count("build_hit");
    let (hits, misses) = p.during(|| assert_eq!(run(&mut spell()).unwrap(), "one:none\n"));
    assert_eq!((hits, misses), (0, 0), "{}", p.stats());
    assert_eq!(p.count("build_hit"), known + 1, "{}", p.stats());
    assert_eq!(p.count("continued"), 4, "{}", p.stats());

    // After a pause the next compilation is recorded again, so that a
    // checkout in long use still adds what it builds to the cache.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    write(p.root.join("leaf/src/text file"), "five");
    let after_pause = run(spell().env("JATO_EDIT_WINDOW", "1")).unwrap();
    assert_eq!(after_pause, "five:none\n");
    assert_eq!(p.count("stored"), stored + 4, "{}", p.stats());
    assert_eq!(p.count("continued"), 4, "{}", p.stats());
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(run(&mut spell()).unwrap(), "five:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}

#[test]
fn on_a_busy_machine_compilations_in_a_built_target_are_not_watched() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // A build made from nothing is recorded however busy the machine is.
    let busy = || {
        let mut cmd = p.command();
        cmd.env_remove("JATO_EDIT_WINDOW").env("JATO_BUSY", "0.001");
        cmd
    };
    success(busy().args(["build"]));
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    assert_eq!(p.count("stored"), 2, "{}", p.stats());
    // Put back and then edited, with every processor in use: the edited
    // crate and the one that depends on it are left to rustc, though
    // neither was compiled here lately.
    p.clean();
    success(busy().args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    edit_leaf(&p.root, "in a hurry");
    let out = success(busy().args(["run", "-q"]));
    assert_eq!(out.stdout, b"in a hurry\n");
    assert_eq!(p.count("stored"), 2, "{}", p.stats());
    assert_eq!(p.count("continued"), 2, "{}", p.stats());
    // rustc was handed the state it saved in the first build all the same.
    assert_eq!(p.count("state_given"), 2, "{}", p.stats());
    // With processors to spare the next crate to be edited is recorded.
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{}!\", leaf::value()); }",
    );
    let out = success(busy().env("JATO_BUSY", "0").args(["run", "-q"]));
    assert_eq!(out.stdout, b"in a hurry!\n");
    assert_eq!(p.count("stored"), 2, "{}", p.stats());
    assert_eq!(p.count("continued"), 3, "{}", p.stats());
}

#[test]
fn a_target_directory_filled_in_from_a_record_is_marked_as_cargo_marks_it() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // The build is recorded in a target directory that another command
    // had already made, so Cargo does not write its mark during the build.
    p.run(&["check", "-q"]);
    let tag = fs::read(p.root.join("target/CACHEDIR.TAG")).unwrap();
    p.run(&["build", "-q"]);
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
    let elsewhere = p.temp.path().join("other/target");
    success(
        p.command()
            .env("CARGO_TARGET_DIR", &elsewhere)
            .args(["build", "-q"]),
    );
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(fs::read(elsewhere.join("CACHEDIR.TAG")).unwrap(), tag);
    assert_eq!(p.stale_for_cargo(&p.root, &elsewhere), 0);
}

#[test]
fn tests_and_programs_start_from_a_recorded_build() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    write(
        p.root.join("leaf/src/lib.rs"),
        "mod value; pub use value::value;\n#[test] fn works() { assert_eq!(value(), \"one:none\"); }",
    );
    write(
        p.root.join("src/main.rs"),
        "fn main() { let unused_marker = 1; println!(\"{}\", leaf::value()); }",
    );
    let passed = |out: &Output| String::from_utf8_lossy(&out.stdout).contains("1 passed");
    let said = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();

    // The first test run makes its build where the build can be recorded
    // whole, showing what is compiled as it goes. Cargo then finds the
    // build done and says the rest once, as it would have alone.
    let first = p.run(&["test", "-p", "leaf", "works", "--", "--test-threads=1"]);
    assert!(passed(&first));
    let text = said(&first);
    assert_eq!(text.matches("Compiling leaf").count(), 1, "{text}");
    assert_eq!(text.matches("Finished").count(), 1, "{text}");
    assert_eq!(text.matches("Running").count(), 1, "{text}");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());

    // The same for a program, whose warnings must not be shown twice.
    let first = p.run(&["run", "arguments", "--for", "the-program"]);
    assert_eq!(first.stdout, b"one:none\n");
    let text = said(&first);
    assert_eq!(text.matches("Compiling app").count(), 1, "{text}");
    assert_eq!(text.matches("unused variable").count(), 1, "{text}");
    assert_eq!(text.matches("Finished").count(), 1, "{text}");
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());

    p.clean();
    // Nothing is compiled or fetched one command at a time: the build the
    // test needs is put back whole, and Cargo then runs the tests. Which
    // tests are asked for does not change the build.
    let (hits, misses) = p.during(|| assert!(passed(&p.run(&["test", "-p", "leaf"]))));
    assert_eq!((hits, misses), (0, 0), "{}", p.stats());
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"one:none\n"));
    assert_eq!((hits, misses), (0, 0), "{}", p.stats());
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    assert_eq!(p.count("build_miss"), 2, "{}", p.stats());
    // Asked for by itself, either build is the one already recorded.
    p.clean();
    p.run(&["test", "-p", "leaf", "--no-run"]);
    p.run(&["build"]);
    assert_eq!(p.count("build_hit"), 4, "{}", p.stats());
    assert_eq!(p.count("build_miss"), 2, "{}", p.stats());

    // A build that fails ahead of the command is the command's to report.
    write(
        p.root.join("src/main.rs"),
        "fn main() { missing_function(); }",
    );
    p.clean();
    let failed = p.command().args(["run"]).output().unwrap();
    assert_eq!(failed.status.code(), Some(101));
    let text = said(&failed);
    assert_eq!(text.matches("cannot find function").count(), 1, "{text}");
}

#[test]
fn compilers_are_not_started_through_another_cache_s_stand_ins() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    if !observes() {
        return;
    }
    let p = Project::new();
    // A directory first on PATH in which `cc` leads to a compiler cache.
    // This one keeps a count of its uses, as such tools do, which no
    // result could be recorded around.
    let tools = p.temp.path().join("tools");
    let shims = p.temp.path().join("shims");
    let counted = p.temp.path().join("uses");
    write(
        tools.join("ccache"),
        &format!(
            "#!/bin/sh\necho used >> '{}'\nexec /usr/bin/cc \"$@\"\n",
            counted.display()
        ),
    );
    fs::set_permissions(tools.join("ccache"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&counted, "");
    fs::create_dir_all(&shims).unwrap();
    for name in ["cc", "gcc", "c++"] {
        symlink(tools.join("ccache"), shims.join(name)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(shims.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let run = || {
        success(
            p.command()
                .env("PATH", &path)
                .env_remove("JATO_PATH")
                .args(["run", "-q"]),
        )
    };
    assert_eq!(run().stdout, b"one:none\n");
    assert_eq!(fs::read(&counted).unwrap(), b"", "the stand-in was used");
    assert_eq!(p.count("uncacheable"), 0, "{}", p.stats());
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(run().stdout, b"one:none\n"));
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
    // A directory that holds anything else is somebody's own and is kept.
    write(shims.join("a-tool-of-mine"), "#!/bin/sh\n");
    p.clean();
    write(
        p.root.join("src/main.rs"),
        "fn main() { println!(\"{}!\", leaf::value()); }",
    );
    assert_eq!(run().stdout, b"one:none!\n");
    assert_ne!(
        fs::read(&counted).unwrap(),
        b"",
        "the directory was left out"
    );
    assert_eq!(p.count("uncacheable"), 1, "{}", p.stats());
}

#[test]
fn a_c_compiler_configured_behind_another_cache_is_named_alone() {
    let p = Project::new();
    // As a compiler cache's setup leaves it for build scripts to find.
    write(
        p.root.join(".cargo/config.toml"),
        "[build]\nincremental = true\n\n[env]\nHOST_CC = \"ccache cc\" # theirs\nHOST_CXX = \"g++\"\n",
    );
    write(
        p.root.join("build.rs"),
        r#"fn main() {
            let read = |name| std::env::var(name).unwrap_or_default();
            let found = format!("{}|{}", read("HOST_CC"), read("HOST_CXX"));
            std::fs::write(std::path::Path::new(&read("OUT_DIR")).join("found.txt"), found).unwrap();
            println!("cargo:rerun-if-env-changed=HOST_CC");
        }"#,
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() { println!("{}", include_str!(concat!(env!("OUT_DIR"), "/found.txt"))); }"#,
    );
    // Further away, where Cargo only looks for what the project leaves
    // unsaid.
    write(
        p.temp.path().join("cargo/config.toml"),
        "[env]\nHOST_CXX = \"ccache c++\"\n",
    );
    // Whatever the environment these tests run in says about compilers.
    let command = || {
        let mut cmd = p.command();
        cmd.env_remove("HOST_CC").env_remove("HOST_CXX");
        cmd
    };
    let found = |cmd: &mut Command| String::from_utf8(success(cmd.args(["run", "-q"])).stdout);
    assert_eq!(found(&mut command()).unwrap(), "cc|g++\n");
    // What the person set for this command themselves is theirs to set.
    assert_eq!(
        found(command().env("HOST_CC", "clang")).unwrap(),
        "clang|g++\n"
    );
    assert_eq!(
        found(command().env("HOST_CC", "sccache clang")).unwrap(),
        "clang|g++\n"
    );
    // Without Jato, Cargo applies its configuration as written.
    assert_eq!(
        found(command().env("JATO_DISABLE", "1")).unwrap(),
        "ccache cc|g++\n"
    );
}

#[test]
fn a_file_that_may_not_be_read_is_known_by_what_can_be_seen_of_it() {
    use std::os::unix::fs::PermissionsExt;
    if !observes() {
        return;
    }
    let p = Project::whole();
    // Somewhere on the machine a build script looks around in, as one
    // that searches for a library does, is a file kept from this user, and
    // a link to something in a directory kept from this user.
    let kept = p.temp.path().join("elsewhere/kept");
    let keep = |text: &str| {
        if kept.exists() {
            fs::set_permissions(&kept, fs::Permissions::from_mode(0o600)).unwrap();
        }
        write(&kept, text);
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o000)).unwrap();
    };
    keep("nobody's business");
    // The link is there, and where it leads cannot even be asked about:
    // to the build script that is as good as nothing being there.
    let closed = p.temp.path().join("elsewhere/closed");
    write(closed.join("inside"), "out of reach");
    let link = p.temp.path().join("elsewhere/link");
    std::os::unix::fs::symlink(closed.join("inside"), &link).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    write(
        p.root.join("build.rs"),
        &format!(
            r#"fn main() {{
                let path = {kept:?};
                let size = std::fs::metadata(path).unwrap().len();
                let _ = std::fs::read(path);
                assert!(std::fs::metadata({link:?}).is_err());
                let to = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("size.txt");
                std::fs::write(to, size.to_string()).unwrap();
            }}"#
        ),
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() { println!("{}", include_str!(concat!(env!("OUT_DIR"), "/size.txt"))); }"#,
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"17\n");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"17\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // What the build script could see of it has changed.
    keep("nobody's business at all");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"24\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
    fs::set_permissions(&kept, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Give the project in `root` a build script that compiles C from the
/// workspace with debugging information, the way the `cc` crate does it,
/// after asking the compiler whether a header is there.
fn compile_c(root: &Path) {
    write(root.join("native/answer.h"), "#define ANSWER 42\n");
    write(
        root.join("native/answer.c"),
        "#include \"answer.h\"\nint answer(void) { return ANSWER; }\n",
    );
    write(
        root.join("native/probe.c"),
        "#include <jato_extra.h>\nint probe(void) { return EXTRA; }\n",
    );
    write(
        root.join("build.rs"),
        r#"use std::process::Command;
        fn main() {
            let out = std::env::var("OUT_DIR").unwrap();
            let cc = |args: &[&str]| {
                Command::new("cc").args(args).status().unwrap().success()
            };
            // Whether the header is installed, as build scripts ask.
            let probe = format!("{out}/probe.o");
            if cc(&["-Iextra", "-c", "native/probe.c", "-o", &probe]) {
                println!("cargo:rustc-cfg=extra");
            }
            let object = format!("{out}/answer.o");
            assert!(cc(&["-g", "-O1", "-c", "native/answer.c", "-o", &object]));
            let library = format!("{out}/libanswer.a");
            let _ = std::fs::remove_file(&library);
            assert!(Command::new("ar").args(["crs", &library, &object]).status().unwrap().success());
            println!("cargo:rustc-link-search=native={out}");
            println!("cargo:rustc-link-lib=static=answer");
            println!("cargo:rerun-if-changed=native");
            println!("cargo:rustc-check-cfg=cfg(extra)");
        }"#,
    );
    write(
        root.join("src/main.rs"),
        r#"unsafe extern "C" { fn answer() -> i32; }
        fn main() {
            let extra = if cfg!(extra) { " and more" } else { "" };
            println!("{}{extra}", unsafe { answer() });
        }"#,
    );
}

/// The files a build script's C compiler left in `target`, by name.
fn compiled_c(target: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![target.join("debug/build")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                dirs.push(entry.path());
            } else if entry.file_name() == name {
                found.push(entry.path());
            }
        }
    }
    found
}

#[test]
fn c_that_a_build_script_compiles_is_remembered_like_rust() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    compile_c(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"42\n");
    // The compilation that worked is kept. The one that failed was an
    // answer to the build script, and is asked again another time.
    assert_eq!(p.count("tool_miss"), 2, "{}", p.stats());
    assert_eq!(p.count("tool_stored"), 1, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // The compiler was told other names for the workspace, so what it
    // wrote for a debugger does not say where this checkout is.
    let objects = compiled_c(&p.root.join("target"), "answer.o");
    assert_eq!(objects.len(), 1, "{objects:?}");
    let object = fs::read(&objects[0]).unwrap();
    let names = |bytes: &[u8], needle: &Path| {
        let needle = needle.as_os_str().as_encoded_bytes();
        bytes.windows(needle.len()).any(|window| window == needle)
    };
    assert!(!names(&object, &p.root), "the object names its checkout");
    assert!(names(&object, Path::new("/proc/self/cwd")));

    // So another checkout is given the whole build.
    let copy = |name: &str| {
        let root = p.temp.path().join(name).join("checkout");
        populate(&root);
        compile_c(&root);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
        root
    };
    let second = copy("second");
    assert_eq!(
        success(p.command_in(&second).args(["run", "-q"])).stdout,
        b"42\n"
    );
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_miss"), 2, "{}", p.stats());

    // In a checkout whose Rust differs, and whose build is not started
    // from the first one's, Cargo carries the build out and runs the build
    // script, whose compiler is given what was kept.
    let third = copy("third");
    let main = fs::read_to_string(third.join("src/main.rs")).unwrap();
    write(
        third.join("src/main.rs"),
        &main.replace("{}{extra}", "{}{extra}!"),
    );
    assert_eq!(
        success(
            p.command_in(&third)
                .env("JATO_NEAR", "0")
                .args(["run", "-q"])
        )
        .stdout,
        b"42!\n"
    );
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_hit"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_miss"), 3, "{}", p.stats());
    assert_eq!(compiled_c(&third.join("target"), "answer.o").len(), 1);

    // A header is as much a part of the compilation as the file naming it.
    write(third.join("native/answer.h"), "#define ANSWER 43\n");
    assert_eq!(
        success(p.command_in(&third).args(["run", "-q"])).stdout,
        b"43!\n"
    );
    assert_eq!(p.count("tool_hit"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_stored"), 2, "{}", p.stats());

    // What the failed compilation looked for is something the whole build
    // depends on, though nothing else in the build looks there: once the
    // header is there, the build is another one.
    let fourth = copy("fourth");
    write(fourth.join("extra/jato_extra.h"), "#define EXTRA 1\n");
    let before = p.count("build_hit");
    assert_eq!(
        success(p.command_in(&fourth).args(["run", "-q"])).stdout,
        b"42 and more\n"
    );
    assert_eq!(p.count("build_hit"), before, "{}", p.stats());
    assert_eq!(p.count("uncacheable"), 0, "{}", p.stats());
}

/// What a build script has compiled is the same work whichever command
/// the build is for. Cargo lists a directory more for the loader when it
/// runs a build script in a build than in a check.
#[test]
fn what_a_build_script_compiled_for_a_check_is_there_for_a_build() {
    if !observes() {
        return;
    }
    let p = Project::new();
    compile_c(&p.root);
    p.run(&["check"]);
    assert_eq!(p.count("tool_stored"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_hit"), 0, "{}", p.stats());
    let built = p.temp.path().join("built");
    success(p.command().env("CARGO_TARGET_DIR", &built).args(["build"]));
    assert_eq!(p.count("tool_hit"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_stored"), 1, "{}", p.stats());
    let out = Command::new(built.join("debug/app")).output().unwrap();
    assert_eq!(out.stdout, b"42\n");
    // And the other way round.
    let q = Project::new();
    compile_c(&q.root);
    q.run(&["build"]);
    success(
        q.command()
            .env("CARGO_TARGET_DIR", q.temp.path().join("checked"))
            .args(["check"]),
    );
    assert_eq!(q.count("tool_hit"), 1, "{}", q.stats());
}

#[test]
fn a_tool_whose_result_cannot_be_kept_is_part_of_the_build_all_the_same() {
    use std::os::unix::fs::PermissionsExt;
    if !observes() {
        return;
    }
    let p = Project::whole();
    // An assembler that says more than is worth keeping, and writes the
    // file it is given.
    let tools = p.temp.path().join("tools");
    write(
        tools.join("yasm"),
        "#!/bin/sh\nhead -c 5000000 /dev/zero | tr '\\0' x\necho assembled > \"$1\"\n",
    );
    fs::set_permissions(tools.join("yasm"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(tools.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    write(
        p.root.join("build.rs"),
        r#"use std::io::Write;
        use std::process::{Command, Stdio};
        fn main() {
            let out = std::env::var("OUT_DIR").unwrap();
            let said = Command::new("yasm").arg(format!("{out}/made.txt")).output().unwrap();
            assert!(said.status.success());
            assert_eq!(said.stdout.len(), 5_000_000);
            // And a compiler that is sent its source, which nobody saw.
            let mut cc = Command::new("cc")
                .args(["-x", "c", "-c", "-", "-o", &format!("{out}/piped.o")])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            cc.stdin.take().unwrap().write_all(b"int piped(void) { return 7; }\n").unwrap();
            assert!(cc.wait().unwrap().success());
        }"#,
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() {
            let object = include_bytes!(concat!(env!("OUT_DIR"), "/piped.o"));
            print!("{} {}", object.len() > 100, include_str!(concat!(env!("OUT_DIR"), "/made.txt")));
        }"#,
    );
    let run = || success(p.command().env("PATH", &path).args(["run", "-q"]));
    assert_eq!(run().stdout, b"true assembled\n");
    // Only the assembler asked for a result, and it could not be kept.
    assert_eq!(p.count("tool_miss"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_uncacheable"), 1, "{}", p.stats());
    assert_eq!(p.count("tool_stored"), 0, "{}", p.stats());
    // What both did is the build's own doing, and the build is kept.
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // That build wrote the lock file. Put back, it finds the file as it
    // should be and leaves it alone: to Cargo the time of the newest file
    // in a package is the state of a build script that names no inputs,
    // and it would run this one again.
    let lock = || {
        fs::metadata(p.root.join("Cargo.lock"))
            .unwrap()
            .modified()
            .unwrap()
    };
    let written = lock();
    p.clean();
    assert_eq!(run().stdout, b"true assembled\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(lock(), written);
    assert_eq!(p.count("tool_miss"), 1, "{}", p.stats());
}

#[test]
fn a_link_by_a_compiler_that_a_build_script_runs_is_that_compiler_s_work() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // A build script that has rustc build a small program, as some do to
    // find out about the machine. rustc starts the C compiler to link it,
    // with its own tools put first on PATH and files in a directory that
    // is named anew each time.
    write(
        p.root.join("build.rs"),
        r#"use std::process::Command;
        fn main() {
            let out = std::env::var("OUT_DIR").unwrap();
            std::fs::write(format!("{out}/probe.rs"), "fn main() { print!(\"probed\"); }").unwrap();
            let built = Command::new(std::env::var("RUSTC").unwrap())
                .args(["--crate-name", "probe", "--crate-type", "bin", "--out-dir", &out])
                .arg(format!("{out}/probe.rs"))
                .status()
                .unwrap();
            assert!(built.success());
            let said = Command::new(format!("{out}/probe")).output().unwrap();
            std::fs::write(format!("{out}/said.txt"), said.stdout).unwrap();
        }"#,
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() { println!("{}", include_str!(concat!(env!("OUT_DIR"), "/said.txt"))); }"#,
    );
    assert_eq!(p.run(&["run", "-q"]).stdout, b"probed\n");
    // No result is kept for the link by itself: it could never be asked
    // for again. The build as a whole is kept, and found again.
    assert_eq!(p.count("tool_miss"), 0, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"probed\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

#[test]
fn a_tool_given_another_path_by_its_build_script_does_not_hide_the_build() {
    use std::os::unix::fs::PermissionsExt;
    if !observes() {
        return;
    }
    let p = Project::whole();
    compile_c(&p.root);
    // An assembler of the project's own, in a directory the build script
    // puts first on PATH for the compiler it starts.
    let tools = p.root.join("tools");
    write(tools.join("as"), "#!/bin/sh\nexec /usr/bin/as \"$@\"\n");
    fs::set_permissions(tools.join("as"), fs::Permissions::from_mode(0o755)).unwrap();
    let script = fs::read_to_string(p.root.join("build.rs")).unwrap();
    write(
        p.root.join("build.rs"),
        &script.replace(
            "Command::new(\"cc\").args(args)",
            "Command::new(\"cc\").env(\"PATH\", format!(\"{}/tools:{}\", std::env::var(\"CARGO_MANIFEST_DIR\").unwrap(), std::env::var(\"PATH\").unwrap())).args(args)",
        ),
    );
    assert_ne!(script, fs::read_to_string(p.root.join("build.rs")).unwrap());
    assert_eq!(p.run(&["run", "-q"]).stdout, b"42\n");
    assert_eq!(p.count("tool_stored"), 1, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    // The compiler looked along a PATH the build never had. What matters
    // to the build is what its own PATH finds, and that is as it was.
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"42\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

/// Whether watched programs tell of their own reads here: the library
/// they load for that is built for one kind of machine.
fn notes() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && std::env::var_os("JATO_NOTES").is_none_or(|v| v != "0")
}

#[test]
fn programs_tell_of_what_they_read_and_are_watched_as_well_without() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    compile_c(&p.root);
    assert_eq!(p.run(&["run", "-q"]).stdout, b"42\n");
    // The compilers said much of what they read themselves. What a
    // program's loader opens before the program runs is always reported.
    let noted = p.count("noted");
    if notes() {
        assert!(noted > 500 && p.count("reported") > 0, "{}", p.stats());
    } else {
        assert_eq!(noted, 0, "{}", p.stats());
    }
    // Watched by the kernel alone, the same build is the same build: the
    // record made one way is found the other, and what is recorded the
    // other way round is found again too.
    let alone = |root: &Path| {
        let mut cmd = p.command_in(root);
        cmd.env("JATO_NOTES", "0");
        cmd
    };
    p.clean();
    assert_eq!(success(alone(&p.root).args(["run", "-q"])).stdout, b"42\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    write(p.root.join("native/answer.h"), "#define ANSWER 44\n");
    p.clean();
    let before = p.count("noted");
    assert_eq!(success(alone(&p.root).args(["run", "-q"])).stdout, b"44\n");
    assert_eq!(p.count("noted"), before, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"44\n");
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
    // And a header the compiler read is still what the result depends
    // on, told of or reported.
    write(p.root.join("native/answer.h"), "#define ANSWER 45\n");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"45\n");
    assert_eq!(p.count("build_hit"), 2, "{}", p.stats());
}

#[test]
fn the_library_in_watched_programs_stands_in_for_nothing_else() {
    if !observes() || !notes() {
        return;
    }
    let p = Project::whole();
    p.run(&["build"]);
    let library = fs::read_dir(p.temp.path().join("cache/lib"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension() == Some(OsStr::new("so")))
        .expect("the library is kept in the cache");
    // Every program Jato watches loads this ahead of its C library, and
    // anything it defined would be taken for the program's own. It may
    // define the functions it stands in for and nothing more: no memory
    // functions, and nothing a Rust program brings itself.
    let Ok(listed) = Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(&library)
        .output()
    else {
        return;
    };
    let defined: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().last().map(str::to_owned))
        .collect();
    assert!(defined.len() > 10, "{defined:?}");
    for name in &defined {
        let stood_in_for = ["open", "stat", "access"]
            .iter()
            .any(|kind| name.trim_start_matches('_').contains(kind));
        assert!(stood_in_for, "{name} is defined for every watched program");
    }
    // And it asks nothing of the system but what every C library has, so
    // that it loads into a program built against another one.
    let needed = Command::new("readelf")
        .arg("-d")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&needed.stdout).contains("NEEDED"),
        "the library names another it needs"
    );
}

#[test]
fn a_program_that_exits_while_its_threads_still_ask_is_recorded() {
    if !observes() {
        return;
    }
    // A program that exits takes its other threads with it, whatever they
    // were doing. One that was waiting for Jato to look at its call never
    // makes the call, and by the time Jato looks there is no thread left
    // to ask what the call named. rustc ends this way with its worker
    // threads still at work.
    let p = Project::whole();
    for round in 1..=3 {
        write(
            p.root.join("build.rs"),
            &format!(
                r#"// Round {round}.
fn main() {{
    let out = std::env::var("OUT_DIR").unwrap();
    for thread in 0..8 {{
        let out = out.clone();
        std::thread::spawn(move || loop {{
            // Calls that change something, which Jato is always shown.
            let dir = format!("{{out}}/made-by-{{thread}}");
            let _ = std::fs::create_dir(&dir);
            let _ = std::fs::remove_dir(&dir);
        }});
    }}
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::process::exit(0);
}}
"#
            ),
        );
        let _ = fs::remove_dir_all(p.root.join("target"));
        // Carried out by Cargo each time, so that the script runs.
        let out = success(p.command().env("JATO_NEAR", "0").args(["run", "-q"]));
        assert_eq!(out.stdout, b"one:none\n");
        assert_eq!(p.count("build_stored"), round, "{}", p.stats());
    }
}

#[test]
fn a_program_that_gives_up_the_way_to_tell_is_watched_all_the_same() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // Something outside the package that nothing but the build script
    // reads, so that only watching the build script can know of it.
    let data = p.temp.path().join("elsewhere/data.txt");
    write(&data, "first");
    // The build script closes every descriptor it was started with above
    // the usual three, as some programs do, opens a file of its own where
    // the one for telling Jato was, and only then reads.
    write(
        p.root.join("build.rs"),
        &format!(
            r#"use std::os::fd::IntoRawFd;
            unsafe extern "C" {{
                fn close(fd: i32) -> i32;
                fn dup2(from: i32, to: i32) -> i32;
            }}
            fn main() {{
                let out = std::env::var("OUT_DIR").unwrap();
                for fd in 3..1024 {{
                    unsafe {{ close(fd) }};
                }}
                let mine = std::fs::File::create(format!("{{out}}/mine.txt")).unwrap().into_raw_fd();
                assert_eq!(unsafe {{ dup2(mine, 217) }}, 217);
                let text = std::fs::read_to_string({data:?}).unwrap();
                assert!(std::fs::metadata("Cargo.toml").unwrap().is_file());
                std::fs::write(format!("{{out}}/data.txt"), text).unwrap();
                println!("cargo:rerun-if-changed=build.rs");
            }}"#
        ),
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() {
            let mine = include_bytes!(concat!(env!("OUT_DIR"), "/mine.txt"));
            println!("{} {}", include_str!(concat!(env!("OUT_DIR"), "/data.txt")), mine.len());
        }"#,
    );
    // Nothing was written into the build script's own file.
    assert_eq!(p.run(&["run", "-q"]).stdout, b"first 0\n");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"first 0\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // What it read after that was seen: with the file changed, the build
    // is another one.
    write(&data, "second");
    p.clean();
    assert_eq!(p.run(&["run", "-q"]).stdout, b"second 0\n");
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
}

#[test]
fn rustdoc_configured_as_another_cache_s_stand_in_is_started_itself() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    if !observes() {
        return;
    }
    let p = Project::whole();
    // As a compiler cache's setup leaves it: Cargo's configuration names a
    // link to the cache as rustdoc. This one keeps a count of its uses, as
    // such tools do, which no build could be recorded around.
    let tools = p.temp.path().join("tools");
    let counted = p.temp.path().join("uses");
    write(
        tools.join("kache"),
        &format!(
            "#!/bin/sh\necho used >> '{}'\nexec rustdoc \"$@\"\n",
            counted.display()
        ),
    );
    fs::set_permissions(tools.join("kache"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&counted, "");
    let stand_in = p.temp.path().join("shims/rustdoc");
    fs::create_dir_all(stand_in.parent().unwrap()).unwrap();
    symlink(tools.join("kache"), &stand_in).unwrap();
    write(
        p.root.join(".cargo/config.toml"),
        &format!(
            "[build]\nincremental = true\nrustdoc = \"{}\"\n",
            stand_in.display()
        ),
    );
    // A build script that asks rustdoc for its version, as those that
    // describe the build to the program do.
    write(
        p.root.join("build.rs"),
        r#"fn main() {
            let rustdoc = std::env::var_os("RUSTDOC").unwrap();
            let out = std::process::Command::new(rustdoc).arg("-V").output().unwrap();
            assert!(out.status.success());
            let to = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("doc.txt");
            std::fs::write(to, out.stdout).unwrap();
        }"#,
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() { print!("{}", include_str!(concat!(env!("OUT_DIR"), "/doc.txt"))); }"#,
    );
    let command = || {
        let mut cmd = p.command();
        cmd.env_remove("RUSTDOC").env_remove("CARGO_BUILD_RUSTDOC");
        cmd
    };
    let version = success(command().args(["run", "-q"])).stdout;
    assert!(version.starts_with(b"rustdoc "), "{version:?}");
    assert_eq!(fs::read(&counted).unwrap(), b"", "the stand-in was used");
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());
    p.clean();
    assert_eq!(success(command().args(["run", "-q"])).stdout, version);
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    // What the person names for this command themselves is looked at in
    // the same way.
    p.clean();
    success(command().env("RUSTDOC", &stand_in).args(["build"]));
    assert_eq!(fs::read(&counted).unwrap(), b"", "the stand-in was used");
    // Without Jato, Cargo applies its configuration as written.
    p.clean();
    success(command().env("JATO_DISABLE", "1").args(["build"]));
    assert_ne!(fs::read(&counted).unwrap(), b"", "Cargo was told otherwise");
}

/// Give `leaf` a body that the compiler has to compile anew.
fn edit_leaf(root: &Path, text: &str) {
    write(
        root.join("leaf/src/value.rs"),
        &format!("pub fn value() -> String {{ String::from({text:?}) }}"),
    );
}

#[test]
fn the_compiler_s_saved_state_is_kept_for_other_checkouts() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // With the pauses a person leaves between edits.
    let command = |root: &Path| {
        let mut cmd = p.command_in(root);
        cmd.env_remove("JATO_EDIT_WINDOW");
        cmd
    };
    let run = |root: &Path| String::from_utf8(success(command(root).args(["run", "-q"])).stdout);
    let sessions = |root: &Path| {
        fs::read_dir(root.join("target/debug/incremental"))
            .map(|crates| crates.flatten().count())
            .unwrap_or(0)
    };
    success(command(&p.root).args(["build"]));
    // Both crates are the workspace's own, which rustc compiles so that it
    // can compile them again faster. What it saved for that is kept.
    assert_eq!(p.count("state_saved"), 2, "{}", p.stats());
    assert_eq!(p.count("state_given"), 0, "{}", p.stats());

    // A second checkout starts from the recorded build. Nothing in it has
    // been compiled there, so rustc has saved nothing there.
    let copy = p.temp.path().join("second/checkout");
    populate(&copy);
    fs::copy(p.root.join("Cargo.lock"), copy.join("Cargo.lock")).unwrap();
    success(command(&copy).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    assert_eq!(sessions(&copy), 0);

    // Its first edit is compiled from the state saved in the first one.
    edit_leaf(&copy, "edited");
    assert_eq!(run(&copy).unwrap(), "edited\n");
    assert_eq!(p.count("state_given"), 2, "{}", p.stats());
    assert_eq!(sessions(&copy), 2);
    // From then on that checkout has state of its own.
    edit_leaf(&copy, "edited again");
    assert_eq!(run(&copy).unwrap(), "edited again\n");
    assert_eq!(p.count("state_given"), 2, "{}", p.stats());
    assert_eq!(p.count("state_saved"), 2, "{}", p.stats());

    // The same in the checkout it came from, once its target is gone.
    p.clean();
    edit_leaf(&p.root, "here");
    assert_eq!(run(&p.root).unwrap(), "here\n");
    assert_eq!(p.count("state_given"), 4, "{}", p.stats());

    // What is kept is replaced by newer state once it has been there a
    // while, so that it follows the sources. The older lot goes.
    let kept = p.stats()["entries"].as_u64().unwrap();
    let third = p.temp.path().join("third/checkout");
    populate(&third);
    fs::copy(p.root.join("Cargo.lock"), third.join("Cargo.lock")).unwrap();
    success(command(&third).args(["build"]));
    edit_leaf(&third, "renewed");
    let out = success(
        command(&third)
            .env("JATO_KEEP_FOR", "0")
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, b"renewed\n");
    assert_eq!(p.count("state_given"), 6, "{}", p.stats());
    assert_eq!(p.count("state_saved"), 4, "{}", p.stats());
    // Two more results, for the two crates compiled, and no more state.
    assert_eq!(
        p.stats()["entries"].as_u64().unwrap(),
        kept + 2,
        "{}",
        p.stats()
    );
    assert_eq!(p.count("damaged"), 0, "{}", p.stats());
    // The next checkout is handed the newer lot.
    let fourth = p.temp.path().join("fourth/checkout");
    populate(&fourth);
    fs::copy(p.root.join("Cargo.lock"), fourth.join("Cargo.lock")).unwrap();
    success(command(&fourth).args(["build"]));
    edit_leaf(&fourth, "after that");
    assert_eq!(run(&fourth).unwrap(), "after that\n");
    assert_eq!(p.count("state_given"), 8, "{}", p.stats());
    assert_eq!(p.count("state_saved"), 4, "{}", p.stats());

    // Asked not to, Jato leaves rustc to start from nothing.
    p.clean();
    edit_leaf(&p.root, "from nothing");
    let out = success(
        command(&p.root)
            .env("JATO_SAVED_STATE", "0")
            .args(["run", "-q"]),
    );
    assert_eq!(out.stdout, b"from nothing\n");
    assert_eq!(p.count("state_given"), 8, "{}", p.stats());
}

#[test]
fn saved_state_follows_a_library_into_a_target_directory_elsewhere() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    // Each checkout builds into a scratch directory of its own that is not
    // inside it, as helpers working on one project are often set up.
    let command = |root: &Path, target: &Path| {
        let mut cmd = p.command_in(root);
        cmd.env_remove("JATO_EDIT_WINDOW")
            .env("CARGO_TARGET_DIR", target);
        cmd
    };
    let first = p.temp.path().join("scratch/first");
    success(command(&p.root, &first).args(["build"]));
    // The library's state is kept: rustc writes a library itself, and can
    // be given its target directory under a name that is the same
    // anywhere. The program's is not, since a linker writes that.
    assert_eq!(p.count("state_saved"), 1, "{}", p.stats());

    let copy = p.temp.path().join("second/checkout");
    populate(&copy);
    fs::copy(p.root.join("Cargo.lock"), copy.join("Cargo.lock")).unwrap();
    let second = p.temp.path().join("scratch/second");
    success(command(&copy, &second).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    edit_leaf(&copy, "edited elsewhere");
    let out = success(command(&copy, &second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"edited elsewhere\n");
    assert_eq!(p.count("state_given"), 1, "{}", p.stats());
    assert_eq!(p.count("uncacheable"), 0, "{}", p.stats());
    // What was compiled this way is a result like any other.
    fs::remove_dir_all(&second).unwrap();
    let (hits, misses) = p.during(|| {
        let out = success(command(&copy, &second).args(["run", "-q"]));
        assert_eq!(out.stdout, b"edited elsewhere\n");
    });
    assert_eq!((hits, misses), (2, 0), "{}", p.stats());
}

#[test]
fn a_compiler_that_gives_up_what_it_inherits_is_told_its_real_directory() {
    use std::os::unix::fs::PermissionsExt;
    if !observes() {
        return;
    }
    let p = Project::whole();
    // The compiler behind a program that closes what it was started with,
    // as a launcher written in some languages does without being asked.
    let closing = p.temp.path().join("tools/rustc");
    write(&closing, "#!/bin/bash\nexec 219<&-\nexec rustc \"$@\"\n");
    fs::set_permissions(&closing, fs::Permissions::from_mode(0o755)).unwrap();
    let command = |root: &Path, target: &Path| {
        let mut cmd = p.command_in(root);
        cmd.env_remove("JATO_EDIT_WINDOW")
            .env("CARGO_TARGET_DIR", target)
            .env("RUSTC", &closing);
        cmd
    };
    let first = p.temp.path().join("scratch/first");
    let out = success(command(&p.root, &first).args(["run", "-q"]));
    assert_eq!(out.stdout, b"one:none\n");
    // Jato found that out before any crate was compiled, and what the
    // compiler saved for an output directory named as it is stays there.
    assert_eq!(p.count("state_saved"), 0, "{}", p.stats());
    let known: Vec<_> = fs::read_dir(p.temp.path().join("cache/locks"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("held-"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect();
    assert_eq!(known, ["no"]);

    let copy = p.temp.path().join("second/checkout");
    populate(&copy);
    fs::copy(p.root.join("Cargo.lock"), copy.join("Cargo.lock")).unwrap();
    let second = p.temp.path().join("scratch/second");
    success(command(&copy, &second).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    edit_leaf(&copy, "edited elsewhere");
    let out = success(command(&copy, &second).args(["run", "-q"]));
    assert_eq!(out.stdout, b"edited elsewhere\n");
    assert_eq!(p.count("state_given"), 0, "{}", p.stats());
    assert_eq!(p.count("uncacheable"), 0, "{}", p.stats());
}

/// The object files rustc keeps for the crate whose name starts with
/// `krate`, counted on either side of the widest gap between the times
/// they were written: those there before, and those written after.
fn object_files(incremental: &Path, krate: &str) -> (usize, usize) {
    let state = fs::read_dir(incremental)
        .unwrap()
        .flatten()
        .find(|dir| dir.file_name().to_string_lossy().starts_with(krate))
        .unwrap();
    let mut times: Vec<std::time::SystemTime> = fs::read_dir(state.path())
        .unwrap()
        .flatten()
        .filter(|session| session.path().is_dir())
        .flat_map(|session| fs::read_dir(session.path()).unwrap().flatten())
        .filter(|file| file.path().extension() == Some(OsStr::new("o")))
        .map(|file| file.metadata().unwrap().modified().unwrap())
        .collect();
    times.sort();
    let gaps = times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]).unwrap());
    let widest = gaps.enumerate().max_by_key(|(_, gap)| *gap);
    match widest {
        // The compiler pauses for longer than this before it starts.
        Some((at, gap)) if gap > std::time::Duration::from_millis(100) => {
            (at + 1, times.len() - at - 1)
        }
        _ => (0, times.len()),
    }
}

#[test]
fn rustc_accepts_and_verifies_saved_state_from_another_checkout() {
    let Some(rustc) = std::env::var_os("JATO_TEST_NIGHTLY") else {
        return;
    };
    let cargo = Path::new(&rustc).with_file_name("cargo");
    let p = Project::whole();
    // The compiler behind a pause. An object file that was there before
    // the compiler started is then much older than one it writes, however
    // quick or busy the machine is.
    let slow = p.temp.path().join("slow/rustc");
    write(
        &slow,
        &format!(
            "#!/bin/sh\nsleep 0.2\nexec '{}' \"$@\"\n",
            Path::new(&rustc).display()
        ),
    );
    fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let rustc = slow;
    // The compiler says when it turns saved state down, and checks every
    // result it takes from it against one computed afresh.
    let command = |root: &Path| {
        let mut cmd = p.command_in(root);
        cmd.env("CARGO", &cargo)
            .env("RUSTC", &rustc)
            .env("RUSTFLAGS", "-Zincremental-info -Zincremental-verify-ich")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("JATO_EDIT_WINDOW");
        cmd
    };
    // Two modules, which rustc compiles to separate object files.
    let sources = |root: &Path| {
        write(
            root.join("src/main.rs"),
            "mod a; mod b; fn main() { println!(\"{}{}\", a::text(), b::text()); }",
        );
        write(
            root.join("src/a.rs"),
            "pub fn text() -> String { leaf::value() }",
        );
        write(
            root.join("src/b.rs"),
            "pub fn text() -> &'static str { \"!\" }",
        );
    };
    let edit = |root: &Path, marks: &str| {
        let (open, close) = marks.split_at(1);
        write(
            root.join("src/a.rs"),
            &format!("pub fn text() -> String {{ format!(\"{open}{{}}{close}\", leaf::value()) }}"),
        )
    };
    let checkout = |name: &str| {
        let root = p.temp.path().join(name).join("checkout");
        populate(&root);
        sources(&root);
        fs::copy(p.root.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
        root
    };
    sources(&p.root);
    success(command(&p.root).args(["build"]));
    assert_eq!(p.count("state_saved"), 2, "{}", p.stats());

    let second = checkout("second");
    success(command(&second).args(["build"]));
    assert_eq!(p.count("build_hit"), 1, "{}", p.stats());
    edit(&second, "<>");
    let built = success(command(&second).args(["build"]));
    let said = String::from_utf8_lossy(&built.stderr);
    assert_eq!(p.count("state_given"), 1, "{}", p.stats());
    assert!(said.contains("[incremental]"), "{said}");
    assert!(!said.contains("ignoring cache"), "{said}");
    assert_eq!(
        success(command(&second).args(["run", "-q"])).stdout,
        b"<one:none>!\n"
    );
    // For a program rustc takes its analysis from the state and generates
    // all code again: it ties generated code to the real paths of the
    // libraries a program is linked with, and those are in another target
    // directory here.

    // With the state as it was saved, rustc turns it down: it was made
    // under another workspace's path. This is what the adjustment is for.
    let third = checkout("third");
    success(command(&third).args(["build"]));
    let from = fs::read_dir(p.root.join("target/debug/incremental")).unwrap();
    for krate in from.flatten() {
        let to = third
            .join("target/debug/incremental")
            .join(krate.file_name());
        fs::create_dir_all(&to).unwrap();
        for entry in fs::read_dir(krate.path()).unwrap().flatten() {
            let dest = to.join(entry.file_name());
            if entry.path().is_dir() {
                fs::create_dir_all(&dest).unwrap();
                for file in fs::read_dir(entry.path()).unwrap().flatten() {
                    fs::copy(file.path(), dest.join(file.file_name())).unwrap();
                }
            } else {
                fs::copy(entry.path(), dest).unwrap();
            }
        }
    }
    edit(&third, "[]");
    let built = success(command(&third).args(["build"]));
    let said = String::from_utf8_lossy(&built.stderr);
    assert!(said.contains("ignoring cache"), "{said}");
    assert_eq!(p.count("state_given"), 1, "{}", p.stats());

    // The same for a library whose target directory is somewhere else and
    // reaches the compiler on a descriptor.
    let outside = |name: &str| p.temp.path().join("scratch").join(name);
    let leaf_edit = |root: &Path, text: &str| {
        write(
            root.join("leaf/src/lib.rs"),
            "mod value; mod other; pub use value::value; pub use other::other;",
        );
        write(
            root.join("leaf/src/other.rs"),
            "pub fn other() -> u32 { 7 }",
        );
        edit_leaf(root, text);
    };
    let fourth = checkout("fourth");
    leaf_edit(&fourth, "kept");
    success(
        command(&fourth)
            .env("CARGO_TARGET_DIR", outside("fourth"))
            .args(["build"]),
    );
    let fifth = checkout("fifth");
    leaf_edit(&fifth, "kept");
    let mut restore = command(&fifth);
    restore.env("CARGO_TARGET_DIR", outside("fifth"));
    success(restore.args(["build"]));
    let given = p.count("state_given");
    leaf_edit(&fifth, "handed on");
    let mut build = command(&fifth);
    build.env("CARGO_TARGET_DIR", outside("fifth"));
    let built = success(build.args(["build"]));
    let said = String::from_utf8_lossy(&built.stderr);
    assert_eq!(p.count("state_given"), given + 1, "{}", p.stats());
    assert!(!said.contains("ignoring cache"), "{said}");
    // The object file of the module that was not edited is the one the
    // state came with, there before the compiler started. The edited
    // module's was written after.
    let (kept, compiled) = object_files(&outside("fifth").join("debug/incremental"), "leaf-");
    assert!(kept > 0 && compiled > 0, "{kept} kept, {compiled} compiled");

    // And for a library whose target directory is inside its checkout.
    let sixth = checkout("sixth");
    leaf_edit(&sixth, "kept inside");
    success(command(&sixth).args(["build"]));
    let seventh = checkout("seventh");
    leaf_edit(&seventh, "kept inside");
    success(command(&seventh).args(["build"]));
    let given = p.count("state_given");
    leaf_edit(&seventh, "handed on inside");
    let built = success(command(&seventh).args(["build"]));
    let said = String::from_utf8_lossy(&built.stderr);
    assert!(p.count("state_given") > given, "{}", p.stats());
    assert!(!said.contains("ignoring cache"), "{said}");
    let (kept, compiled) = object_files(&seventh.join("target/debug/incremental"), "leaf-");
    assert!(kept > 0 && compiled > 0, "{kept} kept, {compiled} compiled");
}

#[test]
fn stopping_the_build_ahead_of_a_test_stops_the_command() {
    use std::io::{BufRead, BufReader, Read};
    if !observes() {
        return;
    }
    let p = Project::whole();
    // A build that takes long enough to be interrupted.
    write(
        p.root.join("build.rs"),
        "fn main() { std::thread::sleep(std::time::Duration::from_secs(8)); }",
    );
    write(
        p.root.join("src/main.rs"),
        "fn main() {}\n#[test] fn never_reached() { println!(\"the test ran\"); }",
    );
    let mut child = p
        .command()
        .args(["test", "--", "--nocapture"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Cargo says so when it starts on the package with the build script.
    let mut progress = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("Compiling app") {
        line.clear();
        assert_ne!(progress.read_line(&mut line).unwrap(), 0, "no build began");
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    success(Command::new("kill").args(["-INT", &child.id().to_string()]));
    let status = child.wait().unwrap();
    let mut printed = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut printed)
        .unwrap();
    assert_eq!(status.code(), Some(130), "{printed}");
    assert!(!printed.contains("the test ran"), "{printed}");
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
// The nearest `.settings/value` in the directories above the crate, the
// way tools look for the settings that apply to a project.
#[proc_macro]
pub fn nearest(_: TokenStream) -> TokenStream {
    let found = std::path::Path::new(&dir())
        .ancestors()
        .skip(1)
        .find_map(|above| std::fs::read_to_string(above.join(".settings/value")).ok());
    literal(found.unwrap_or_default())
}
"#,
    );
}

#[test]
fn settings_found_above_a_crate_are_found_again_from_where_it_is_kept() {
    if !observes() {
        return;
    }
    // A workspace kept inside a directory whose name is how the settings
    // it looks for begin, as the crates Cargo downloads are kept inside
    // its home directory, where its settings are.
    let p = Project::named("top/.settings/ws");
    let ready = |root: &Path, value: &str| {
        add_macros(root);
        write(
            root.join("leaf/Cargo.toml"),
            "[package]\nname='leaf'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmacros={path='../macros'}\n",
        );
        write(root.join("leaf/src/lib.rs"), "pub use macros::*;");
        write(
            root.join("src/main.rs"),
            "fn main() { println!(\"{}\", leaf::nearest!()); }",
        );
        write(root.parent().unwrap().join("value"), value);
    };
    ready(&p.root, "from above");
    assert_eq!(p.run(&["run", "-q"]).stdout, b"from above\n");
    // The same crate, compiled where it is, finds the same settings.
    p.clean();
    let (hits, misses) = p.during(|| assert_eq!(p.run(&["run", "-q"]).stdout, b"from above\n"));
    assert_eq!((hits, misses), (3, 0), "{}", p.stats());
    // A second workspace below the same settings, further down, finds
    // them too, and is given the result.
    let below = p.temp.path().join("top/.settings/deeper/ws");
    populate(&below);
    ready(&below, "unused");
    fs::remove_file(below.parent().unwrap().join("value")).unwrap();
    fs::copy(p.root.join("Cargo.lock"), below.join("Cargo.lock")).unwrap();
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&below).args(["run", "-q"]));
        assert_eq!(out.stdout, b"from above\n");
    });
    assert_eq!((hits, misses), (3, 0), "{}", p.stats());
    // Settings nearer to that workspace are found first.
    write(
        below.parent().unwrap().join(".settings/value"),
        "from nearer",
    );
    let touch = |root: &Path| {
        let main = root.join("src/main.rs");
        let text = fs::read_to_string(&main).unwrap();
        write(&main, &format!("{text} "));
        write(&main, &text);
    };
    touch(&below);
    assert_eq!(
        success(p.command_in(&below).args(["run", "-q"])).stdout,
        b"from nearer\n"
    );
    // A copy somewhere else has other files above it, whatever they say:
    // the crate that looked is compiled there.
    let other = p.temp.path().join("other/.settings/ws");
    populate(&other);
    ready(&other, "from elsewhere");
    fs::copy(p.root.join("Cargo.lock"), other.join("Cargo.lock")).unwrap();
    let (hits, misses) = p.during(|| {
        let out = success(p.command_in(&other).args(["run", "-q"]));
        assert_eq!(out.stdout, b"from elsewhere\n");
    });
    assert_eq!((hits, misses), (2, 1), "{}", p.stats());
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

/// Make the project an application over twelve libraries that Cargo can
/// compile all at once.
fn twelve_parts(p: &Project) {
    let mut manifest =
        String::from("[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\n");
    let mut main = String::from("fn main() { let mut n = 0;\n");
    for i in 0..12 {
        manifest.push_str(&format!("part{i}={{path='part{i}'}}\n"));
        main.push_str(&format!("n += part{i}::value();\n"));
        write(
            p.root.join(format!("part{i}/Cargo.toml")),
            &format!("[package]\nname='part{i}'\nversion='0.1.0'\nedition='2021'\n"),
        );
        write(
            p.root.join(format!("part{i}/src/lib.rs")),
            &format!("pub fn value() -> u32 {{ {i} }}"),
        );
    }
    main.push_str("println!(\"{n}\"); }");
    write(p.root.join("Cargo.toml"), &manifest);
    write(p.root.join("src/main.rs"), &main);
}

#[test]
fn builds_sharing_a_cache_take_turns_at_the_processors() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    twelve_parts(&p);
    // One place: Cargo starts twelve compilers and they run one at a time,
    // whether a build's supervisor hands out the work or each compiler's
    // wrapper is on its own.
    success(
        p.command()
            .env("JATO_JOBS", "1")
            .args(["build", "-j", "12"]),
    );
    let stats = p.stats();
    assert_eq!(stats["counters"]["stored"], 13, "{stats}");
    let queued = stats["counters"]["queued"].as_u64().unwrap_or(0);
    assert!(queued >= 6, "{stats}");
    for i in 0..12 {
        write(
            p.root.join(format!("part{i}/src/lib.rs")),
            &format!("pub fn value() -> u32 {{ {i} + 1 }}"),
        );
    }
    let out = success(
        p.command()
            .env("JATO_JOBS", "1")
            .env("JATO_WHOLE_BUILDS", "0")
            .args(["run", "-q", "-j", "12"]),
    );
    assert_eq!(out.stdout, b"78\n");
    let stats = p.stats();
    assert_eq!(stats["counters"]["stored"], 26, "{stats}");
    assert!(
        stats["counters"]["queued"].as_u64().unwrap_or(0) >= queued + 6,
        "{stats}"
    );
    // Without a limit nothing waits.
    p.clean();
    for i in 0..12 {
        write(
            p.root.join(format!("part{i}/src/lib.rs")),
            &format!("pub fn value() -> u32 {{ {i} + 2 }}"),
        );
    }
    let before = p.count("queued");
    success(
        p.command()
            .env("JATO_JOBS", "0")
            .args(["build", "-j", "12"]),
    );
    assert_eq!(p.count("queued"), before, "{}", p.stats());
}

#[test]
fn compilations_finishing_together_in_one_build_are_all_stored() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    twelve_parts(&p);
    p.run(&["build", "-j", "12"]);
    let stats = p.stats();
    assert_eq!(stats["counters"]["stored"], 13, "{stats}");
    assert_eq!(
        stats["counters"]["uncacheable"].as_u64().unwrap_or(0),
        0,
        "{stats}"
    );
    assert_eq!(stats["counters"]["build_stored"], 1, "{stats}");
    p.clean();
    let (hits, misses) = p.during(|| {
        let out = success(
            p.command()
                .env("JATO_WHOLE_BUILDS", "0")
                .args(["run", "-q", "-j", "12"]),
        );
        assert_eq!(out.stdout, b"66\n");
    });
    assert_eq!((hits, misses), (13, 0), "{}", p.stats());
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

/// Real-world build script cases across checkouts:
/// 1. A build script invoking `git rev-parse HEAD` (like ripgrep/cargo/rust-analyzer/nushell)
///    still starts from an earlier build in a new git worktree (rerunning only that script).
/// 2. A build script printing `cargo:rerun-if-changed=<abs path>` relocates Cargo's binary
///    `dep-*` fingerprint so Cargo does not rerun the script after a restore in another checkout.
/// 3. A build script rewriting `conftest.c` between `cc -c` runs (like autoconf/jemalloc)
///    is still recorded as a whole build.
/// 4. A `cc`-style hashed object name (`<16-hex>-foo.o`) hits cache under a different hash.
#[test]
fn real_world_build_scripts_work_across_worktrees() {
    if !observes() {
        return;
    }
    // 1. Build script calling git rev-parse HEAD + absolute rerun-if-changed path.
    let p = Project::whole();
    write(p.root.join("data.txt"), "hello");
    write(
        p.root.join("build.rs"),
        r#"use std::process::Command;
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let abs = format!("{dir}/data.txt");
    println!("cargo:rerun-if-changed={abs}");
    let data = std::fs::read_to_string(&abs).unwrap();
    let rev = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    println!("cargo:rustc-env=GIT_REV={rev}");
    println!("cargo:rustc-env=DATA_VAL={}", data.trim());
}
"#,
    );
    write(
        p.root.join("src/main.rs"),
        r#"fn main() { println!("{} {} {}", env!("GIT_REV"), env!("DATA_VAL"), leaf::value()); }"#,
    );
    write(p.root.join(".gitignore"), "/target\n");
    p.run(&["cargo", "generate-lockfile"]);
    git(&p.root, &["init", "-q", "-b", "main"]);
    git(&p.root, &["config", "user.email", "t@example.com"]);
    git(&p.root, &["config", "user.name", "t"]);
    git(&p.root, &["add", "."]);
    git(&p.root, &["commit", "-q", "-m", "init"]);

    let rev1 = String::from_utf8(
        success(
            Command::new("git")
                .current_dir(&p.root)
                .args(["rev-parse", "--short=8", "HEAD"]),
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let out1 = p.run(&["run", "-q"]);
    assert_eq!(
        String::from_utf8_lossy(&out1.stdout),
        format!("{rev1} hello one:none\n")
    );
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());

    // A new worktree at the same commit: `.git` is a file pointing to `a/.git/worktrees/wt1`.
    // The build MUST start from `p.root`'s recorded build (`build_near == 1`) instead of cold.
    let wt1 = p.temp.path().join("wt1");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt1.to_str().unwrap(),
            "HEAD",
        ],
    );
    let out_wt1 = success(p.command_in(&wt1).args(["run", "-q"]));
    assert_eq!(
        String::from_utf8_lossy(&out_wt1.stdout),
        format!("{rev1} hello one:none\n")
    );
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());

    // A third worktree with an edit to `leaf`: MUST also start from a recorded build.
    let wt2 = p.temp.path().join("wt2");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt2.to_str().unwrap(),
            "HEAD",
        ],
    );
    write(wt2.join("leaf/src/text file"), "edited");
    let out_wt2 = success(p.command_in(&wt2).args(["run", "-q"]));
    assert_eq!(
        String::from_utf8_lossy(&out_wt2.stdout),
        format!("{rev1} hello edited:none\n")
    );
    assert_eq!(p.count("build_near"), 2, "{}", p.stats());

    // 2. Absolute `rerun-if-changed`: build in `c1`, restore in `c2`, remove `c1`,
    // and verify Cargo alone sees 0 stale crates in `c2`!
    let q = Project::whole();
    add_side(&q.root);
    success(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&q.root),
    );
    let setup_abs = |root: &Path| {
        write(root.join("data.txt"), "v1");
        write(
            root.join("build.rs"),
            r#"fn main() {
    let abs = format!("{}/data.txt", std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed={abs}");
    let v = std::fs::read_to_string(&abs).unwrap();
    println!("cargo:rustc-env=DATA_VAL={}", v.trim());
}
"#,
        );
        write(
            root.join("src/main.rs"),
            r#"fn main() { println!("{}", env!("DATA_VAL")); }"#,
        );
    };
    let c1 = checkout(&q, "c1");
    setup_abs(&c1);
    assert_eq!(
        success(q.command_in(&c1).args(["run", "-q"])).stdout,
        b"v1\n"
    );
    let c2 = checkout(&q, "c2");
    setup_abs(&c2);
    let hits = q.count("build_hit");
    assert_eq!(
        success(q.command_in(&c2).args(["run", "-q"])).stdout,
        b"v1\n"
    );
    assert_eq!(q.count("build_hit"), hits + 1, "{}", q.stats());
    fs::remove_dir_all(&c1).unwrap();
    assert_eq!(
        q.stale_for_cargo(&c2, &c2.join("target")),
        0,
        "absolute rerun-if-changed should be relocated in dep-*"
    );

    // 3 & 4. Build script rewriting `conftest.c` + compiling a hashed `-o <16-hex>-foo.o`.
    let r = Project::whole();
    add_side(&r.root);
    success(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&r.root),
    );
    let setup_c = |root: &Path, hash_hex: &str| {
        write(
            root.join("build.rs"),
            &format!(
                r#"use std::process::Command;
fn main() {{
    let out = std::env::var("OUT_DIR").unwrap();
    // Simulate autoconf rewriting conftest.c between two cc calls.
    let conf = format!("{{out}}/conftest.c");
    std::fs::write(&conf, "int f(void) {{ return 1; }}\n").unwrap();
    let st1 = Command::new("cc").args(["-c", &conf, "-o", &format!("{{out}}/c1.o")]).status().unwrap();
    assert!(st1.success());
    std::fs::write(&conf, "int g(void) {{ return 2; }}\n").unwrap();
    let st2 = Command::new("cc").args(["-c", &conf, "-o", &format!("{{out}}/c2.o")]).status().unwrap();
    assert!(st2.success());
    let _ = std::fs::remove_file(&conf);
    // Simulate cc crate hashing OUT_DIR into the .o filename.
    let gen = format!("{{out}}/gen.c");
    std::fs::write(&gen, "int answer(void) {{ return 42; }}\n").unwrap();
    let obj = format!("{{out}}/{hash_hex}-gen.o");
    let st3 = Command::new("cc").args(["-c", &gen, "-o", &obj]).status().unwrap();
    assert!(st3.success());
    let lib = format!("{{out}}/libgen.a");
    let st4 = Command::new("ar").args(["crs", &lib, &obj]).status().unwrap();
    assert!(st4.success());
    println!("cargo:rustc-link-search=native={{out}}");
    println!("cargo:rustc-link-lib=static=gen");
}}
"#
            ),
        );
        write(
            root.join("src/main.rs"),
            "unsafe extern \"C\" { fn answer() -> i32; }\nfn main() { println!(\"{}\", unsafe { answer() }); }\n",
        );
    };
    let s1 = checkout(&r, "s1");
    setup_c(&s1, "1111222233334444");
    assert_eq!(
        success(r.command_in(&s1).args(["run", "-q"])).stdout,
        b"42\n"
    );
    assert_eq!(r.count("build_stored"), 1, "{}", r.stats());
    // In a second checkout with a DIFFERENT 16-hex hash prefix, compiling `gen.c` MUST hit cache!
    let s2 = checkout(&r, "s2");
    setup_c(&s2, "aaaabbbbccccdddd");
    let tool_hits_before = r.count("tool_hit");
    let mut cmd = r.command_in(&s2);
    cmd.env("JATO_WHOLE_BUILDS", "0").args(["run", "-q"]);
    assert_eq!(success(&mut cmd).stdout, b"42\n");
    assert!(
        r.count("tool_hit") > tool_hits_before,
        "hashed cc -o should hit cache under another 16-hex prefix: {}",
        r.stats()
    );
}

/// Recompiling an incremental library from identical sources produces a
/// different `.rlib` byte-for-byte on rustc (random CGU session suffix),
/// yet downstream linking must still hit the cache without relinking.
#[test]
fn downstream_binary_hits_cache_across_recompiled_incremental_rlib() {
    let Some(rustc) = std::env::var_os("JATO_TEST_NIGHTLY") else {
        return;
    };
    if !observes() {
        return;
    }
    let cargo = Path::new(&rustc).with_file_name("cargo");
    let p = Project::new();
    let run_in = |target: &Path, extra_env: Option<(&str, &str)>| {
        let mut cmd = p.command();
        cmd.env("CARGO", &cargo)
            .env("RUSTC", &rustc)
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_INCREMENTAL", "1")
            .env_remove("RUSTUP_TOOLCHAIN");
        if let Some((k, v)) = extra_env {
            cmd.env(k, v);
        }
        cmd.args(["run", "-q"]);
        success(&mut cmd)
    };
    // Build once with Jato into `t1` (records `leaf` and `app`).
    let t1 = p.temp.path().join("t1");
    assert_eq!(run_in(&t1, None).stdout, b"one:none\n");

    // Build `leaf` fresh into `t2` with a separate empty cache so rustc
    // runs with the same `--remap-path-prefix` flags and generates a fresh
    // random incremental CGU session suffix inside `libleaf-*.rlib`.
    let t2 = p.temp.path().join("t2");
    let mut fresh = p.command();
    fresh
        .env("CARGO", &cargo)
        .env("RUSTC", &rustc)
        .env("CARGO_TARGET_DIR", &t2)
        .env("JATO_CACHE_DIR", p.temp.path().join("other-cache"))
        .env("CARGO_INCREMENTAL", "1")
        .env_remove("RUSTUP_TOOLCHAIN")
        .args(["build", "-p", "leaf", "-q"]);
    success(&mut fresh);

    // Confirm rustc actually wrote different raw bytes for `libleaf-*.rlib`.
    let find_rlib = |dir: &Path| -> Vec<u8> {
        let mut stack = vec![dir.to_owned()];
        while let Some(d) = stack.pop() {
            for entry in fs::read_dir(d).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|n| n.starts_with("libleaf-") && n.ends_with(".rlib"))
                {
                    return fs::read(&path).unwrap();
                }
            }
        }
        panic!("no libleaf rlib in {}", dir.display());
    };
    assert_ne!(
        find_rlib(&t1),
        find_rlib(&t2),
        "rustc incremental builds should produce distinct raw .rlib bytes"
    );

    // Now build `app` in `t2` WITH Jato's original cache: `app` links against
    // the freshly compiled `libleaf-*.rlib` and MUST get a cache hit!
    let (hits, misses) = p.during(|| {
        assert_eq!(run_in(&t2, None).stdout, b"one:none\n");
    });
    assert!(
        hits >= 1 && misses == 0,
        "expected app to hit cache across recompiled incremental rlib: hits={hits} misses={misses} {}",
        p.stats()
    );
}

#[test]
fn an_empty_target_is_seeded_from_another_command_s_build_and_unused_parts_are_pruned() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    add_side(&p.root);
    // Give `leaf` a build script and add a doctest + unit test to `leaf`.
    let setup = |root: &Path| {
        write(
            root.join("leaf/build.rs"),
            r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/gen.rs"), "pub fn extra() -> u32 { 7 }\n").unwrap();
}
"#,
        );
        write(
            root.join("leaf/src/lib.rs"),
            r#"include!(concat!(env!("OUT_DIR"), "/gen.rs"));
/// ```
/// assert_eq!(leaf::extra(), 7);
/// ```
pub fn value() -> &'static str { include_str!("text file").trim() }
#[test]
fn unit_ok() { assert_eq!(extra(), 7); }
"#,
        );
    };
    setup(&p.root);
    success(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&p.root),
    );

    // 1. Record `jato build --workspace` in `c1`.
    let c1 = checkout(&p, "c1");
    setup(&c1);
    success(p.command_in(&c1).args(["build", "--workspace", "-q"]));
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());

    // 2. In a brand-new checkout `c2` (empty target), run `jato test -p leaf --no-run`.
    // It MUST seed from `c1`'s `build --workspace` without rerunning `leaf`'s build script!
    let c2 = checkout(&p, "c2");
    setup(&c2);
    let log2 = p.temp.path().join("c2.log");
    success(
        p.command_in(&c2)
            .env("JATO_LOG", &log2)
            .args(["test", "-p", "leaf", "--no-run", "-q"]),
    );
    let log2_text = fs::read_to_string(&log2).unwrap();
    assert_eq!(p.count("build_seeded"), 1, "{log2_text}\n{}", p.stats());
    assert_eq!(p.count("build_stored"), 2, "{log2_text}\n{}", p.stats());
    assert!(
        !log2_text.contains("[build_script_build]"),
        "seeded target should reuse leaf's build script without compiling/running it:\n{log2_text}"
    );
    assert!(
        !c2.join("target/debug/app").exists(),
        "unused app binary from seed build should be pruned"
    );

    // 3. In a third checkout `c3`, `jato test -p leaf --no-run` hits whole-build cache!
    let c3 = checkout(&p, "c3");
    setup(&c3);
    let hits = p.count("build_hit");
    success(
        p.command_in(&c3)
            .args(["test", "-p", "leaf", "--no-run", "-q"]),
    );
    assert_eq!(p.count("build_hit"), hits + 1, "{}", p.stats());

    // 4. In a fourth checkout `c4` (empty target), `jato test --doc -p leaf`
    // warms/seeds the library ahead of Cargo and passes without rerunning build.rs!
    let c4 = checkout(&p, "c4");
    setup(&c4);
    let log4 = p.temp.path().join("c4.log");
    let doc_out = success(
        p.command_in(&c4)
            .env("JATO_LOG", &log4)
            .args(["test", "--doc", "-p", "leaf"]),
    );
    let log4_text = fs::read_to_string(&log4).unwrap();
    assert!(
        String::from_utf8_lossy(&doc_out.stdout).contains("1 passed"),
        "{}",
        String::from_utf8_lossy(&doc_out.stdout)
    );
    assert!(
        !log4_text.contains("[build_script_build]"),
        "test --doc should seed leaf's build script from the workspace build:\n{log4_text}"
    );

    // 5. In a fifth checkout `c5` with an edited `leaf`, `jato run -p app`
    // seeds `side` & `leaf`'s build script, recompiles only `leaf` + `app`.
    let c5 = checkout(&p, "c5");
    setup(&c5);
    write(c5.join("leaf/src/text file"), "updated");
    let log5 = p.temp.path().join("c5.log");
    let seeded_before = p.count("build_seeded");
    assert_eq!(
        success(
            p.command_in(&c5)
                .env("JATO_LOG", &log5)
                .args(["run", "-p", "app", "-q"]),
        )
        .stdout,
        b"updated 5\n"
    );
    let log5_text = fs::read_to_string(&log5).unwrap();
    assert!(
        p.count("build_seeded") > seeded_before,
        "expected run -p app to seed from workspace build:\n{log5_text}\n{}",
        p.stats()
    );
    assert!(
        !log5_text.contains("[side]") && !log5_text.contains("[build_script_build]"),
        "expected side and build script to be reused from seed:\n{log5_text}"
    );
}

#[test]
fn a_build_script_writing_a_pinned_output_still_lets_another_checkout_start_from_it() {
    if !observes() {
        return;
    }
    let p = Project::whole();
    add_side(&p.root);
    let setup = |root: &Path| {
        write(
            root.join("leaf/build.rs"),
            r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    // Write a binary file embedding OUT_DIR (like jemalloc's libjemalloc.a)
    // and read a generated header via `sub/../gen.h`.
    let sub = format!("{out}/sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(format!("{out}/gen.h"), "42").unwrap();
    let val = std::fs::read_to_string(format!("{sub}/../gen.h")).unwrap();
    let mut bin = vec![0u8, 1, 2, 3];
    bin.extend_from_slice(out.as_bytes());
    std::fs::write(format!("{out}/pinned.bin"), bin).unwrap();
    std::fs::write(format!("{out}/val.rs"), format!("pub const V: u32 = {val};\n")).unwrap();
}
"#,
        );
        write(
            root.join("leaf/src/lib.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/val.rs\"));\npub fn value() -> u32 { V }\n",
        );
        write(
            root.join("src/main.rs"),
            "fn main() { println!(\"{} {}\", leaf::value(), side::side()); }\n",
        );
    };
    setup(&p.root);
    success(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&p.root),
    );
    let c1 = checkout(&p, "c1");
    setup(&c1);
    assert_eq!(
        success(p.command_in(&c1).args(["run", "-q"])).stdout,
        b"42 5\n"
    );
    assert_eq!(p.count("build_stored"), 1, "{}", p.stats());

    // In `c2`, the build MUST start from `c1` (`build_near == 1`), rerunning only
    // `leaf`'s build script rather than falling back to a full cold build!
    let c2 = checkout(&p, "c2");
    setup(&c2);
    assert_eq!(
        success(p.command_in(&c2).args(["run", "-q"])).stdout,
        b"42 5\n"
    );
    assert_eq!(p.count("build_near"), 1, "{}", p.stats());
    assert_eq!(p.count("build_stored"), 2, "{}", p.stats());
}
