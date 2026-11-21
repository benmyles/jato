# Jato

Jato speeds up repeat Rust builds and gives you compiler feedback on save. It remembers individual Cargo compiler results across cleans and branch switches. In a workspace, it checks the package you edited first, then checks the whole workspace.

On a Linux development machine, a clean-target check of ripgrep 14.1.1 took **6.28 s with Cargo and 0.65 s with a warm Jato cache**. Its release rebuild went from **61.61 s to 25.62 s**. These are three-run medians. The benchmark includes first builds, fresh builds, and small edits. [Measurements and reproduction](bench/README.md)

## Get started

Build Jato on Linux with Rust 1.89 or later and a C toolchain. SQLite and Zstandard are bundled. From this repository, run

```sh
cargo install --locked --path .
```

From a Rust project, use the same arguments you would give Cargo.

```sh
jato check --all-targets
jato build --release
jato test -p parser -- --nocapture
jato watch
jato stats
```

The first compilation populates the cache. Cargo continues to skip fresh units and maintains its incremental data in the usual target directory. A restored unit can save an entire compiler invocation. An edit that produces new inputs still needs compilation.

## Work on save

| Command | Behavior |
| --- | --- |
| `jato watch` | Run an initial workspace check. After a member edit, check its package and then the workspace, including dependents and unified features. |
| `jato watch --focus-only` | Give feedback on the edited members. The initial run and shared configuration changes still check the workspace. |
| `jato watch -- test --workspace` | Run these Cargo arguments on every save. |
| `jato watch --watch ../schemas -- check --all-targets` | Watch an additional file or directory for changes. |
| `jato watch --debounce 40` | Start feedback after 40 ms of quiet. The default is 100 ms. |

The watcher uses native filesystem events and Cargo metadata. It subscribes to workspace directories, member directories, and direct local path dependencies. Use `--watch` for other inputs such as an external generator's templates. It honors directory ignore rules and excludes Cargo outputs and Jato's cache. New saves supersede an active job, including its compiler and test processes. Ctrl-C stops the watcher and its active job. Set `JATO_LOG=debug` to see which paths scheduled a check.

## Use with existing tools

`jato cargo` accepts arbitrary Cargo arguments, including toolchain selectors, aliases, and installed subcommands.

```sh
jato cargo +stable check --workspace
jato cargo nextest run
```

To use the cache with Cargo directly or with tools that invoke Cargo, set the wrapper for that process.

```sh
RUSTC_WRAPPER="$(command -v jato)" cargo build --release
```

For persistent use in a project, add the following to its `.cargo/config.toml` and put Jato on `PATH`.

```toml
[build]
rustc-wrapper = "jato"
```

Jato's Cargo commands select Jato as `RUSTC_WRAPPER` for the child process. Cargo can still invoke a nested workspace wrapper such as Clippy. Tests, build scripts, and final linking execute normally.

## Storage and diagnostics

| Setting or command | Purpose |
| --- | --- |
| `JATO_CACHE_DIR` | Private directory on a local filesystem with locking. Defaults to `$XDG_CACHE_HOME/jato`, or `~/.cache/jato`. |
| `JATO_MAX_SIZE=512MiB` | Set the budget for compressed chunks and charged record overhead. The default is 1 GiB. |
| `jato stats --json` | Read machine-wide totals for this cache, including logical artifact bytes, compressed payload, SQLite file sizes, hits, compiler runs, and bypasses. |
| `jato gc 256MiB` | Immediately evict least-recently-used entries to this size. `jato gc` uses the configured budget. `jato gc 0` removes all managed entries. |
| `JATO_LOG=debug jato check` | Print per-crate decisions, including unavailable inputs and unsupported compiler options, when Cargo invokes rustc. |
| `JATO_DISABLE=1 jato build` | Delegate every invocation directly to the compiler for this run. |

Cache work runs in short-lived compiler wrapper processes. Each wrapper streams artifacts in 256 KiB chunks, compresses them independently, and shares identical chunks across entries. A SQLite transaction publishes the whole output set and applies the budget. The database, write-ahead log, temporary staging space, and Cargo's live target directory have additional filesystem costs. The cache contains local compiler artifacts and diagnostics, which can include values embedded by `env!`.

Hits verify each chunk and each reconstructed file before publishing independently writable outputs and replaying diagnostics. Storage failures fall back to compilation. The cache counters begin after Cargo's own freshness check and cover invocations that reach the wrapper. `reused_compiler_ms` sums earlier compiler durations, which can overlap during parallel builds.

## What can be reused

Jato caches stable rustc metadata checks and eligible Rust libraries. Keys include the working directory, compiler arguments, toolchain and sysroot file identities, compiler environment, source and included-file contents, rustc's tracked environment inputs, and dependency artifact hashes. Familiar input manifests allow direct hits. If the module search directories change, Jato asks rustc to rediscover the inputs before trusting a candidate.

Jato preserves absolute paths, including output paths. Reuse works with a stable checkout and target location across cleans, edits, and branch switches. Separate worktrees and relocated target directories generally produce separate entries. The artifact store still deduplicates identical chunks.

Final binaries, procedural macros and their unverified consumers, custom drivers, unfamiliar flags, and native codegen inputs take the compiler path. Metadata-only results can certify other checks. Code generation requires its own evidence. Crates whose dependencies were built before Jato observed them can take a normal compile until those dependencies are recorded. A hit leaves the current rustc incremental directory available. After a clean, rustc starts rebuilding incremental state on the next miss.

## Design references

- Cargo's freshness checks and rustc's incremental compiler already shorten ordinary edits. Jato keeps them enabled and spends its storage budget on reusable compiler outputs. [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)
- Compiler caches must account for hidden inputs, path-sensitive artifacts, and mutable output files. These constraints informed Jato's dependency checks, typed search paths, and independently writable restores. [sccache Rust support](https://github.com/mozilla/sccache/blob/main/docs/Rust.md) · [ccache manual](https://ccache.dev/manual/latest.html) · [kache](https://github.com/kunobi-ninja/kache) · [rustc search paths](https://doc.rust-lang.org/rustc/command-line-arguments.html#-l-add-a-directory-to-the-library-search-path)
- CI archives and container layers reuse builds at a different granularity. Jato records individual successful units, so a feature change in one crate can leave many other entries usable. [rust-cache](https://github.com/Swatinem/rust-cache) · [cargo-chef](https://github.com/LukeMathWalker/cargo-chef)
- Watch tools make checks continuous. Jato adds package-first feedback, a workspace follow-up, and cancellation to its compiler cache. Cargo keeps control of linkers and test runners. [Bacon](https://dystroy.org/bacon/) · [Cargo Watch](https://github.com/watchexec/cargo-watch) · [cargo-nextest](https://nexte.st/docs/design/how-it-works/) · [mold](https://github.com/rui314/mold)

## Development

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. The integration suite builds disposable Cargo projects and exercises compiler invalidation, exact artifact restoration, process cancellation, and concurrent cache access. Project conventions, the release checklist, and the work journal live alongside the code. [AGENTS.md](AGENTS.md) · [TODO.md](TODO.md) · [JOURNAL.md](JOURNAL.md)
