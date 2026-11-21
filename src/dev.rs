//! Cargo owns profile selection and output layout. These settings apply to
//! development builds explicitly requested through `jato dev`.
use std::ffi::OsString;
use std::process::Command;

pub const HELP: &str = "\
Usage
  jato dev [build|run|test|check|clippy] [CARGO_ARGUMENTS...]
  jato dev watch [WATCH_OPTIONS...]
  jato dev worktree [WORKTREE_OPTIONS...] ACTION

With no action, build the workspace's default members. The jato-dev Cargo
profile inherits dev, uses 16 codegen units, keeps workspace line tables, and
omits debug information for dependencies outside the workspace. Outputs use
target/jato-dev. An explicit --release or --profile takes precedence.

Examples
  jato dev
  jato dev run -p cli -- --version
  jato dev test --workspace
";

const PROFILE: [&str; 4] = [
    "profile.jato-dev.inherits=\"dev\"",
    "profile.jato-dev.codegen-units=16",
    "profile.jato-dev.debug=\"line-tables-only\"",
    "profile.jato-dev.package.\"*\".debug=\"none\"",
];

pub fn args(cmd: &mut Command, args: &[OsString]) {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut words = args[..end].iter();
    let verb = loop {
        let Some(word) = words.next() else { break None };
        match word.to_str() {
            Some("--config" | "--color" | "-Z" | "-C") => {
                words.next();
            }
            Some(s) if s.starts_with('-') || s.starts_with('+') => (),
            other => break other,
        }
    };
    if !matches!(
        verb,
        Some(
            "build"
                | "b"
                | "check"
                | "c"
                | "run"
                | "r"
                | "test"
                | "t"
                | "clippy"
                | "rustc"
                | "doc"
                | "d"
                | "rustdoc"
                | "clean"
                | "bench"
        )
    ) {
        cmd.args(args);
        return;
    }
    let prefix = usize::from(
        args.first()
            .is_some_and(|a| a.as_encoded_bytes().starts_with(b"+")),
    );
    cmd.args(&args[..prefix]);
    for setting in PROFILE {
        cmd.args(["--config", setting]);
    }
    cmd.args(&args[prefix..end]);
    let explicit = args[..end].iter().any(|a| {
        a == "--release"
            || a == "--profile"
            || a.as_encoded_bytes().starts_with(b"--profile=")
            || a.to_str()
                .and_then(|s| s.strip_prefix('-'))
                .is_some_and(|s| s.trim_start_matches(['q', 'v']).starts_with('r'))
    });
    if !explicit {
        cmd.args(["--profile", "jato-dev"]);
    }
    cmd.args(&args[end..]);
}
