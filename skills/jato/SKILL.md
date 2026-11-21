---
name: jato
description: Use Jato in Rust Cargo projects to speed up repeated builds and tests, check edits on save, reuse builds across Git worktrees, or diagnose compiler cache misses. Use when setting up Jato or running these development loops with Jato available.
---

# Use Jato in a Cargo project

Start in the same directory where the project runs Cargo. Check `jato --version` when first setting up. To install Jato on Linux, run `cargo install --locked --path /path/to/jato` with the path to the **Jato source checkout**. Building it requires Rust 1.89+ and a C toolchain. If Jato is unavailable and the task is to check or test code, run the project's original Cargo command.

## Run the project's command

Keep the project's toolchain, features, target, profile, package selections, lockfile flags and arguments following `--`. Replace the leading `cargo` with `jato cargo` to preserve an entire invocation, including aliases and installed subcommands. Common commands also have shortcuts.

| Project command | Jato invocation |
| --- | --- |
| `cargo check --workspace --locked` | `jato check --workspace --locked` |
| `cargo test -p parser parses_numbers -- --nocapture` | `jato test -p parser parses_numbers -- --nocapture` |
| `cargo clippy --all-targets -- -D warnings` | `jato clippy --all-targets -- -D warnings` |
| `cargo +stable build --release` | `jato cargo +stable build --release` |

For a tool that starts Cargo itself, prefix its command with `RUSTC_WRAPPER="$(command -v jato)"`. Ordinary Jato commands need no configuration edits. If the task calls for persistent project integration, merge `rustc-wrapper = "jato"` into `[build]` in `.cargo/config.toml`. Check the role of any existing wrapper before replacing it. Every developer and CI runner using that config needs `jato` on `PATH`.

## Choose an edit loop

- For a quick local executable or test, use `jato dev run` or `jato dev test`. These select a separate `jato-dev` profile with workspace line tables and its own target outputs. Use the project's normal profile for CI parity or debugging local variables.
- For feedback on save, run `jato watch`. It initially checks the workspace, then checks an edited package before its workspace follow-up. To run package tests on every save, use `jato watch -- test -p parser`. Stop the watcher you started with Ctrl-C when the session is done.
- For repeated builds in independent checkouts on Linux with working bubblewrap, run `jato worktree --root . build --release` from each checkout. Choose a root that includes relative path dependencies and share one private local `JATO_CACHE_DIR` outside that root. Use `jato worktree --root . run` when the program opens paths embedded by Cargo.

## Explain cache behavior

Keep the target directory and incremental state between edits. Cargo skips fresh units before Jato runs, so an immediate repeat can add zero cache hits. Compare `jato stats --json` before and after the workload. Use `JATO_LOG=debug jato check` for per-crate decisions or `JATO_LOG=timings jato build` to distinguish compilation, validation and restore time. Cargo still runs build scripts and tests. A bypass delegates to the compiler. To limit the cache budget to 512 MiB, set `JATO_MAX_SIZE=512MiB` on the build.
