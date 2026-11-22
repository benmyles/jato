//! What a compiler command's arguments and environment say about it.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What the arguments say about a rustc command. None of it decides whether
/// a result is right; it only says where to look and what to leave out.
#[derive(Default)]
pub struct Facts {
    pub name: String,
    pub source: Option<PathBuf>,
    /// A path inside the target directory.
    pub hint: Option<PathBuf>,
    pub incremental: Option<PathBuf>,
    pub search: Vec<PathBuf>,
    pub reads_stdin: bool,
    pub width: Option<u16>,
    pub ansi: bool,
    pub json: bool,
    pub native_cpu: bool,
    pub keeps_real_paths: bool,
    /// The crates named on the command line.
    pub externs: Vec<PathBuf>,
    /// The command produces something other than a Rust library: a
    /// program, a test, or a library with other crates' code in it.
    pub links: bool,
    /// Arguments left out of the command's identity, and the one whose
    /// colour setting is.
    pub skipped: Vec<usize>,
    pub json_at: Option<usize>,
    /// Where the source file, the output directory and the directory for
    /// incremental state stand among the arguments: the argument that
    /// holds each path, which for the last two may follow `name=`.
    pub source_at: Option<usize>,
    pub out_dir_at: Option<usize>,
    pub incremental_at: Option<usize>,
}

const TAKES_VALUE: &[&str] = &[
    "--crate-name",
    "--crate-type",
    "--edition",
    "--emit",
    "--print",
    "--cfg",
    "--check-cfg",
    "-L",
    "-l",
    "--extern",
    "--sysroot",
    "--out-dir",
    "-o",
    "--explain",
    "--target",
    "-C",
    "--codegen",
    "-Z",
    "-A",
    "--allow",
    "-W",
    "--warn",
    "-D",
    "--deny",
    "-F",
    "--forbid",
    "--force-warn",
    "--cap-lints",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
    "--remap-path-scope",
    "--env-set",
];

impl Facts {
    pub fn parse(args: &[OsString], cwd: &Path) -> Self {
        let mut facts = Facts::default();
        let mut widths = 0;
        let mut width_tokens = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let at = i;
            let Some(arg) = args[i].to_str() else {
                i += 1;
                continue;
            };
            i += 1;
            // Split `--flag=value`, `-Cvalue` and `--flag value` alike.
            let (flag, value, wide) = if TAKES_VALUE.contains(&arg) {
                let value = args.get(i).and_then(|v| v.to_str());
                i += 1;
                (arg, value, true)
            } else if let Some((flag, value)) = arg
                .split_once('=')
                .filter(|(flag, _)| flag.starts_with("--") && TAKES_VALUE.contains(flag))
            {
                (flag, Some(value), false)
            } else if let Some(flag) = ["-C", "-L", "-Z"].iter().find(|f| arg.starts_with(**f)) {
                (*flag, Some(&arg[2..]), false)
            } else {
                if arg == "-" {
                    facts.reads_stdin = true;
                } else if arg == "--test" {
                    facts.links = true;
                } else if !arg.starts_with('-') && arg.ends_with(".rs") && facts.source.is_none() {
                    facts.source = Some(cwd.join(arg));
                    facts.source_at = Some(at);
                }
                continue;
            };
            let Some(value) = value else { continue };
            let tokens = if wide { vec![at, at + 1] } else { vec![at] };
            match flag {
                "--crate-name" => facts.name = value.to_owned(),
                "--crate-type" => facts.links |= !matches!(value, "lib" | "rlib"),
                "--extern" => {
                    if let Some((_, path)) = value.split_once('=') {
                        facts.externs.push(cwd.join(path));
                    }
                }
                "--out-dir" => {
                    facts.hint = Some(cwd.join(value));
                    facts.out_dir_at = tokens.last().copied();
                }
                "-o" if facts.hint.is_none() => {
                    facts.hint = cwd.join(value).parent().map(Path::to_owned)
                }
                "-L" => {
                    let dir = value.split_once('=').map_or(value, |(_, dir)| dir);
                    facts.search.push(cwd.join(dir));
                }
                "-C" | "--codegen" => {
                    if let Some(dir) = value.strip_prefix("incremental=") {
                        facts.incremental = Some(cwd.join(dir));
                        facts.incremental_at = tokens.last().copied();
                        facts.skipped.extend(tokens);
                    } else if value == "target-cpu=native" {
                        facts.native_cpu = true;
                    } else if value.starts_with("instrument-coverage") {
                        facts.keeps_real_paths = true;
                    }
                }
                "--cfg" if value == "trybuild" => facts.keeps_real_paths = true,
                "--diagnostic-width" => {
                    widths += 1;
                    width_tokens = tokens;
                    facts.width = value.parse::<u16>().ok().filter(|w| *w > 0);
                }
                "--json" => {
                    facts.ansi |= value.split(',').any(|v| v == "diagnostic-rendered-ansi");
                    facts.json_at = Some(*tokens.last().unwrap());
                }
                "--error-format" => facts.json = value == "json",
                _ => (),
            }
        }
        // A repeated or malformed width is rustc's to reject, so it stays
        // part of the command.
        if widths == 1 && facts.width.is_some() {
            facts.skipped.extend(width_tokens);
        } else {
            facts.width = None;
        }
        facts.skipped.sort_unstable();
        if facts.name.is_empty() {
            facts.name = "rustc".into();
        }
        facts
    }

    /// A command that compiles a crate, as opposed to one that asks the
    /// compiler a question.
    pub fn compiles(&self) -> bool {
        self.source.is_some() && !self.reads_stdin
    }
}

/// Environment variables that are part of a command's identity. A proc
/// macro or a linker can read any variable without the compiler knowing, so
/// those that such programs are known to read are always included; a crate's
/// own `env!` uses are added from the compiler's dependency output.
pub fn keyed(name: &[u8]) -> bool {
    const EXACT: &[&[u8]] = &[
        b"HOME",
        b"LANG",
        b"TARGET",
        b"HOST",
        b"PROFILE",
        b"OPT_LEVEL",
        b"DEBUG",
        b"OUT_DIR",
        b"SOURCE_DATE_EPOCH",
        b"ZERO_AR_DATE",
        b"LIBRARY_PATH",
        b"LD_LIBRARY_PATH",
        b"LD_PRELOAD",
        b"LD_RUN_PATH",
        b"COMPILER_PATH",
        b"GCC_EXEC_PREFIX",
        b"CPATH",
        b"C_INCLUDE_PATH",
        b"CPLUS_INCLUDE_PATH",
        b"MACOSX_DEPLOYMENT_TARGET",
        b"SDKROOT",
        b"AR",
        b"ARFLAGS",
    ];
    const PREFIXES: &[&[u8]] = &[
        b"CARGO_",
        b"RUSTC",
        b"RUSTDOC",
        b"RUSTFLAGS",
        b"RUST_",
        b"RUSTUP_",
        b"CLIPPY_",
        b"DEP_",
        b"LC_",
        b"CC",
        b"CXX",
        b"CFLAGS",
        b"CPPFLAGS",
        b"LDFLAGS",
        b"PKG_CONFIG",
        b"MOLD_",
        b"LLD_",
    ];
    // Set by Cargo or the user for Cargo itself. They reach the compiler as
    // arguments, or say how this particular build is being run.
    const NOT: &[&[u8]] = &[
        b"CARGO_MAKEFLAGS",
        b"CARGO_PRIMARY_PACKAGE",
        b"CARGO_TARGET_DIR",
        b"CARGO_BUILD_TARGET_DIR",
        b"CARGO_BUILD_BUILD_DIR",
        b"CARGO_BUILD_JOBS",
        b"CARGO_INCREMENTAL",
        b"CARGO_LOG",
        b"RUSTC_WRAPPER",
        b"RUSTC_WORKSPACE_WRAPPER",
        b"RUSTC_LOG",
        b"RUST_LOG",
        b"RUST_BACKTRACE",
        b"RUST_LIB_BACKTRACE",
    ];
    const NOT_PREFIXES: &[&[u8]] = &[b"CARGO_TERM_", b"CARGO_PROFILE_", b"CARGO_ALIAS_"];
    if NOT.contains(&name) || NOT_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments_that_do_not_change_the_result_are_left_out() {
        let list = args(&[
            "--crate-name",
            "a",
            "src/lib.rs",
            "--error-format=json",
            "--json=diagnostic-rendered-ansi,artifacts",
            "--diagnostic-width=120",
            "-C",
            "incremental=/t/debug/incremental",
            "--out-dir",
            "/t/debug/deps",
            "-L",
            "dependency=/t/debug/deps",
            "-Lnative=/usr/lib",
        ]);
        let facts = Facts::parse(&list, Path::new("/w"));
        assert_eq!(facts.name, "a");
        assert_eq!(facts.source.as_deref(), Some(Path::new("/w/src/lib.rs")));
        assert_eq!(facts.width, Some(120));
        assert!(facts.ansi && facts.json && facts.compiles());
        assert_eq!(facts.skipped, [5, 6, 7]);
        assert_eq!(facts.json_at, Some(4));
        assert_eq!(facts.hint.as_deref(), Some(Path::new("/t/debug/deps")));
        assert_eq!(
            facts.search,
            [PathBuf::from("/t/debug/deps"), PathBuf::from("/usr/lib")]
        );
        assert_eq!(
            facts.incremental.as_deref(),
            Some(Path::new("/t/debug/incremental"))
        );
        assert!(!facts.links && facts.externs.is_empty());
        let facts = Facts::parse(
            &args(&[
                "--crate-type",
                "lib",
                "--crate-type=cdylib",
                "--extern",
                "a=/t/liba.rmeta",
                "--extern",
                "proc_macro",
                "x.rs",
            ]),
            Path::new("/w"),
        );
        assert!(facts.links);
        assert_eq!(facts.externs, [PathBuf::from("/t/liba.rmeta")]);
        assert!(Facts::parse(&args(&["--test", "x.rs"]), Path::new("/w")).links);
    }

    #[test]
    fn a_width_rustc_would_reject_stays_in_the_command() {
        for list in [
            args(&["--diagnostic-width=0", "x.rs"]),
            args(&["--diagnostic-width=80", "--diagnostic-width=90", "x.rs"]),
            args(&["--diagnostic-width", "wide", "x.rs"]),
        ] {
            let facts = Facts::parse(&list, Path::new("/w"));
            assert_eq!(facts.width, None, "{list:?}");
            assert!(facts.skipped.is_empty(), "{list:?}");
        }
        let facts = Facts::parse(
            &args(&["--diagnostic-width", "80", "x.rs"]),
            Path::new("/w"),
        );
        assert_eq!((facts.width, &facts.skipped[..]), (Some(80), &[0, 1][..]));
    }

    #[test]
    fn a_query_is_not_a_compilation() {
        let facts = Facts::parse(
            &args(&["-", "--crate-name", "___", "--print=cfg"]),
            Path::new("/w"),
        );
        assert!(facts.reads_stdin && !facts.compiles());
        assert!(!Facts::parse(&args(&["-vV"]), Path::new("/w")).compiles());
    }

    #[test]
    fn only_variables_tools_read_are_part_of_a_command() {
        for name in [
            "CARGO_PKG_NAME",
            "RUSTC_BOOTSTRAP",
            "DEP_Z_INCLUDE",
            "CC",
            "HOME",
            "LC_ALL",
        ] {
            assert!(keyed(name.as_bytes()), "{name}");
        }
        for name in [
            "CARGO_MAKEFLAGS",
            "PATH",
            "TERM",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
            "PWD",
        ] {
            assert!(!keyed(name.as_bytes()), "{name}");
        }
    }
}
