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
    // A file added anywhere in the package is a change to it: Cargo would
    // run the build script again.
    for added in ["notes/another.txt", "extra.txt"] {
        let before = p.count("build_hit");
        write(p.root.join(added), "more");
        p.run(&["build"]);
        assert_eq!(p.count("build_hit"), before, "{added}: {}", p.stats());
    }
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

    // In a checkout whose Rust differs, Cargo carries the build out and
    // runs the build script, whose compiler is given what was kept.
    let third = copy("third");
    let main = fs::read_to_string(third.join("src/main.rs")).unwrap();
    write(
        third.join("src/main.rs"),
        &main.replace("{}{extra}", "{}{extra}!"),
    );
    assert_eq!(
        success(p.command_in(&third).args(["run", "-q"])).stdout,
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
