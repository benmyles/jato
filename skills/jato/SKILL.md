---
name: jato
description: Use Jato in Rust Cargo projects to restore builds that were made before (in this checkout, another worktree or after a clean), keep several builds from overloading the machine, check edits on save, or find out why something was compiled again. Use when setting up Jato or when building, testing or running a Cargo project where Jato is available.
---

# Use Jato in a Cargo project

Start in the directory where the project runs Cargo. Check `jato --version` when first setting up. To install Jato on Linux, run `cargo install --locked --path /path/to/jato` with the path to the **Jato source checkout**. Building it requires Rust 1.89+ and a C toolchain. If Jato is unavailable and the task is to check or test code, run the project's original Cargo command.

## Run the project's command

Keep the project's toolchain, features, target, profile, package selections, lockfile flags and the arguments after `--`. Replace the leading `cargo` with `jato` for build, check, clippy, test, bench and run, or with `jato cargo` for anything else, including a `+toolchain` selector.

| Project command | Jato invocation |
| --- | --- |
| `cargo check --workspace --locked` | `jato check --workspace --locked` |
| `cargo test -p parser parses_numbers -- --nocapture` | `jato test -p parser parses_numbers -- --nocapture` |
| `cargo clippy --all-targets -- -D warnings` | `jato clippy --all-targets -- -D warnings` |
| `cargo +stable build --release` | `jato cargo +stable build --release` |

Run through `jato`, a build that was made before with the same sources, settings and toolchain is put back whole without starting Cargo: in a new worktree, in another target directory, or after `cargo clean`. A build that has to be made is recorded as it runs. Use the same command each time; `jato build -p a` and `jato build -p b` are different builds.

For a tool that starts Cargo itself, prefix its command with `RUSTC_WRAPPER="$(command -v jato)"`. Single compilations are then reused, which is slower for a large restored build than `jato build` but needs no other change. If the task calls for persistent project integration, merge `rustc-wrapper = "jato"` into `[build]` in `.cargo/config.toml`. Check the role of any existing wrapper before replacing it. Every developer and CI runner using that config needs `jato` on `PATH`.

## What to expect

- The first build of a state compiles and records. Later builds of that state, anywhere on the machine that shares the cache, are restored.
- After editing, Cargo and rustc compile what changed at their usual speed. Undoing an edit returns to a recorded build at once.
- In a target that was restored, the first edit of a crate is compiled incrementally all the same: Jato hands rustc the state it saved where the build was recorded. `jato stats` counts this as `state_given`.
- Builds started at the same time through Jato take turns at the processors. A build that seems to wait is queued behind others; `jato stats` shows `queued`.
- Jato records on Linux (x86-64 and aarch64, kernel 5.5 or later). Elsewhere its commands run Cargo unchanged.

## Choose an edit loop

- For a quick local executable or test, use `jato dev run` or `jato dev test`. These select a separate `jato-dev` profile with workspace line tables and its own target outputs. Use the project's normal profile for CI parity or to inspect local variables in a debugger.
- For feedback on save, run `jato watch`. It checks the workspace, then after each save the edited package first and the workspace after it. To run package tests on every save, use `jato watch -- test -p parser`. Stop the watcher you started with Ctrl-C when the session is done.

## Explain cache behavior

Compare `jato stats --json` before and after a command: `build_hit` counts builds put back whole, `hit` single compilations reused, `miss` compilations run and recorded, `continued` compilations left to rustc during a run of edits, `uncacheable` those that could not be stored. `JATO_LOG=debug jato build` says for each command whether it was reused and, if not, which file, variable or setting had changed. Give `JATO_LOG` an absolute path to collect this in a file.

A procedural macro that reads an environment variable needs it named: `JATO_KEY_ENV=NAME,OTHER`. Files and directories a macro or build script reads are seen without help. A compilation that uses the network is never stored.

To inspect the cache budget, run `jato limit`; to set it for every project using that cache, `jato limit 32GiB`. `jato gc 10GiB` evicts least-recently-used results down to a size.
