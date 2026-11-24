# Jato

Jato remembers Rust builds and tests. A build that has been made before, with the same sources, settings and toolchain, is put back without starting Cargo: after `cargo clean`, in another target directory, in a git worktree created a moment ago. A build of sources that differ a little from ones built before starts from that build, and Cargo compiles what changed. A new Cargo command in an empty target directory (`jato test -p foo`, `jato clippy`, `jato build` after `jato check`) seeds from a compatible recorded build of the workspace and prunes unused outputs when done. Passing, hermetic test runs are cached across re-runs and worktrees, and workspace test binaries run in parallel across available cores. Builds that do have to compile go at Cargo's speed (or faster, with asynchronous recording and a named jobserver pipe), reuse every compilation that still applies, the C that build scripts compile included, and take turns at the processors when several are started together.

Checking and building all 121 packages of Olo (1,502 Rust compilations, 113 build scripts compiling ~1,100 C/C++/asm files, 56 dev binaries, 20.6 GiB) on a 32-core AMD EPYC 9655 build server (ext4), Jato 0.18.0 and kache 2.3.0 started side by side:

| Seconds (ext4, side by side) | `check`: Jato 0.18 | `check`: kache 2.3 | `build`: Jato 0.18 | `build`: kache 2.3 |
| --- | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | **153.9** | 160.6 | **480.4** | 482.3 |
| The same command again (72k inputs) | **0.13** | 0.59 | **0.15** | 1.84 |
| Target directory removed | **1.52** | 11.85 | **3.06** | 30.13 |
| A target directory never used | **0.99** | 11.41 | **5.19** | 138.71 |
| Sources copied to another path | **0.52** | 10.37 | **18.21** | 22.22 |
| Function added to `olo-system` in that copy | **3.34** | 9.41 | **54.62** | 78.67 |
| New worktree with that edit before first build | **6.14** | 20.99 | **72.70** | 89.17 |
| New worktree with `Cargo.toml` changed | **3.80** | 10.72 | **10.06** | 19.59 |
| First edit in the restored target | **4.38** | 7.19 | **18.24** | 35.98 |
| The edits undone | **0.26** | 2.96 | 15.27 | 11.40 |
| Disk store / idle daemon RAM | **1.3 GiB / 0** | 1.8 GiB / 115 MiB | **6.2 GiB / 0** | 12.3 GiB / 115 MiB |

With Jato **0.19.0** on the same 32-core EPYC 9655 server (ext4, single-contender runs against the `0.18.0` baseline):
- Olo `check --workspace` (69,220 inputs, 13,088 files, 1.57 GiB): `fresh` **0.057 s** (was 0.125 s, 2.2× faster), `restore` **0.371 s** (was 0.468 s), `copy` **0.382 s** (was 0.687 s, 1.8× faster), `undo` **0.183 s** (was 0.240 s).
- Olo `build --workspace` (72,527 inputs, 13,912 files, 20.9 GiB / 56 binaries): `fresh` **0.071 s** (was 0.146 s, 2.1× faster), `restore` **1.53 s** (1.10 s quiet, was 2.62 s), `new_target` **1.25 s** (0.97 s quiet, was 3.18 s, 2.5–3.3× faster), `copy` **6.41 s** (was 8.50 s), `copy_manifest` **7.55 s** (was 12.99 s), `edit_first` **10.62 s** (9.55 s quiet, was 14.75 s), `undo` **0.69 s** (0.59 s quiet, was 8.94 s, **13–15× faster**). Across the 8 cache-served steps, total time dropped **40%** (61.8 s → 37.3 s).
- With `jato memory 64GiB` on ext4 (`0.19.0`), a full **20.9 GiB** verified restore into a fresh target (`new_target`) finishes in **0.73 s (28.7 GB/s effective throughput)**, `restore` in **1.27 s**, `copy` in **5.59 s**, `fresh` in **0.073 s**, and `undo` in **0.74 s**.

On six open-source Rust workspaces in new git worktrees (`check --workspace`, ext4): `ripgrep` 0.1 s, `rust-analyzer` 0.4 s, `cargo` 0.7 s, `tokio` 0.4 s, `nushell` 2.5 s, `ruff` 9.1 s (5 of 529 compilations). On `ripgrep`, `jato test --workspace` in a brand-new worktree takes **1.68 s** (restored build + cached tests) against **2.97 s** for bare `cargo test --workspace` in an already-built target. [All measurements, with conditions and reproduction commands](bench/README.md)

## Get started

Build Jato on Linux with Rust 1.89 or later and a C toolchain. SQLite and Zstandard are bundled, and so are the small preload libraries watched programs load (built with Jato on Linux x86-64 and aarch64). Install directly from GitHub or from a local checkout:

```sh
cargo install --locked --git https://github.com/benmyles/jato
# or from a local checkout:
cargo install --locked --path .
```

In a Rust project, give Jato the arguments you would give Cargo.

```sh
jato build --release
jato test -p parser -- --nocapture
jato check --all-targets
jato clippy -- -D warnings
jato run
jato cargo +nightly build        # anything else Cargo does
jato stats
```

The first build of a state compiles and is recorded. After that, the same command in the same state is restored instead of built, wherever on the machine it is asked for. An edit is compiled by Cargo and rustc as usual, and undoing the edit returns to the recorded build at once.

Make a project's first build with Jato in an empty target directory: after `cargo clean`, or in a new worktree. In a target directory that Cargo or another cache has already filled, Cargo finds everything up to date and nothing is compiled that Jato could keep. That build is recorded for its own directory alone, and `jato stats` says so.

Jato records on Linux, on x86-64 and aarch64, with kernel 5.5 or later. On other systems its commands run Cargo unchanged.

### Use Jato with coding agents

From the Jato checkout, install the [Jato skill](skills/jato/SKILL.md) for every project in your account. One command installs it for all three clients and requires only Python 3.

```sh
./scripts/install-skill
```

| Client | Example prompt | Default skill directory |
| --- | --- | --- |
| Claude Code | `/jato run the workspace tests` | `~/.claude/skills/jato/` |
| Codex | `$jato run the workspace tests` | `~/.agents/skills/jato/` |
| Antigravity CLI (`agy`) | `/jato run the workspace tests` | `~/.gemini/antigravity-cli/skills/jato/` |

The installer honors `CLAUDE_CONFIG_DIR`. A repeat installation preserves matching files. To update copies that differ, review them and rerun `./scripts/install-skill --force`.

Agents that each work in a checkout of their own are what Jato is built for. A new worktree starts from a restored build, the agents share one store instead of each keeping a full target directory's worth of unique data, and their builds queue for the processors instead of running a full set of compilers each.

## How it works

Jato knows nothing about what rustc computes. A **result** is the files a command wrote and what it printed. It can stand in for a later run of the same command line when everything the earlier run examined is unchanged.

What a command examined is observed, not worked out from its arguments. The command runs under a seccomp filter that reports each file it opens, each path it asks about and each program it starts to Jato, which lets the call continue. A directory opened for reading counts as listed.

Being asked about every call costs a program a turn at the processor each time, which on a busy machine is most of what watching costs. So the programs tell of most of it themselves. Each loads a small library ahead of its C library, which stands in for the functions that open a file for reading or ask about a path: it sends Jato a note of the path and makes the call from one fixed address, and the filter lets a call from that address through if it is of a kind that only reads. Every other call is reported as before: one that creates, writes, renames or removes, for which Jato has to see what was there first, and any call at all from a program that does not load the library or reaches the kernel some other way. Nothing depends on the library being there, only how many calls have to wait. A check of `olo-system` makes about 2,600 such calls, of which the kernel now reports 160. `JATO_NOTES=0` has the kernel report everything. A procedural macro that reads a file, a linker that searches a directory and a build script that starts a compiler are all seen the same way. Environment variables are the one thing a program reads unseen: for those Jato keys every result on the variables Cargo and rustc are known to act on, takes the ones a crate reads through `env!` from the compiler's own report, and the ones a build script declares from Cargo's records. A variable a procedural macro reads with `std::env::var` has to be named in `JATO_KEY_ENV`.

A lookup hashes the command line and its keyed environment, finds the results recorded for that command, and checks one after another whether every file, link, directory listing and variable it depended on is as it was. Files are compared by content; an unchanged file is recognized by its size and timestamps and not read again.

### Whole builds

`jato build`, `check`, `clippy`, and `test` or `bench` with `--no-run` are looked up as one result. On a hit Cargo is not started: the files it would have left in the target directory are put there, those already holding the right bytes are left alone, and Cargo's summary and any compiler warnings are printed as they were. Cargo afterwards finds everything in that target directory up to date.

Where a build is shown is no part of which build it is. Cargo has the compilers colour their diagnostics whether or not it will show them in colour, and leaves the colours out where there is no terminal. Jato does the same with a build it puts back, so one an agent made is put back for a person at a terminal and the other way round. Warnings keep the layout of the terminal their crate was compiled on, as they do when Cargo itself finds a crate up to date.

On a miss Cargo runs under the observer, with each compiler it starts asking Jato for its result or reporting it. When the build ends it is recorded as a whole, depending on what Cargo and its compilers examined outside what the build itself made.

`jato test`, `bench` and `run` first get the build they need: restored if it is recorded, made on its own if not, showing only what is being compiled. Then Cargo runs the command you asked for and finds the build done. A test name filter and a program's arguments are not part of the build.

### One target directory, several commands, and cross-command seeding

A check, a build, clippy and the tests of a workspace share one target directory, and Cargo uses for each what the others made: build scripts and procedural macros are built once, and clippy checks dependencies exactly as `cargo check` does. A build made where another command's recorded build already is, with all of that build's inputs as they were, is recorded as resting on it. The record holds that build's files with its own, under everything either depended on, and is put back whole in a worktree where the other command was never run.

When a command has never been recorded before and runs in an empty target directory (such as `jato test -p foo`, `jato clippy`, or `jato build` in a fresh worktree after only `jato check --workspace` or `jato build --workspace` was recorded), Jato seeds the target directory from a compatible recorded whole build of the same toolchain and profile, leaving out stale compilers and build scripts. Once Cargo completes, any outputs from the seed build that Cargo never asked about are pruned so the resulting target directory and recorded manifest match a build from nothing. `JATO_SEED=0` disables cross-command seeding.

### Cached and parallel tests (`jato test`)

`jato test` first restores or builds the test binaries (or the package library for `jato test --doc`), then intercepts compiled test binary executions through `CARGO_TARGET_<TRIPLE>_RUNNER` while keeping Cargo's exact environment, working directory, and arguments. Each test binary run is keyed by the binary's BLAKE3 digest, arguments, working directory, and filtered full environment, and observed via seccomp + the fast read-note library so any fixture file, directory listing, symlink, shared library, or child process it touches is recorded automatically. Passing (`exit 0`), network-free, external-socket-free, survivor-free, side-effect-free test runs (allowing scratch temp files removed before exit) are cached and reused across re-runs and worktrees. When running multiple test binaries across a workspace (without `--nocapture`, `--test-threads=1`, or `--list`), Jato runs them concurrently across available cores while presenting deterministic Cargo-formatted output. `JATO_CACHE_TESTS=0` disables test caching and `JATO_PARALLEL_TESTS=0` runs test binaries serially. Real user runners (such as `qemu` or `valgrind`) are left untouched.

### Builds that start from an earlier one

The first build in a new worktree is often of sources nobody has built: a commit that is new, or an edit made before anything was built there. No recorded build stands for it. Cargo then has to carry the build out, with every compiler it starts answered from the cache, and for a workspace of some size that takes a while even when one file differs: checking the 121 packages of Olo in a new worktree took 23 s and 75 processor-seconds with one line changed in one program, and 37 s with a function added to a crate that 54 others use.

Such a build starts from the nearest recorded one instead. The record of a whole build says who examined each of its inputs: Cargo itself, a build script or something a build script started, or one of the compilers Cargo started. If everything that differs was examined by those compilers alone, the earlier build's files are put in the target directory without the files those compilers wrote. Cargo finds them missing, runs those compilers, then whatever uses what they make, and finds the rest up to date. The same two builds took 7.2 s and 11.6 s, at a load of 18 to 25, and the next worktree with either change got its build whole in 2 s.

File times play no part in which compilers run again. A changed file that looks older than the earlier build is compiled, and so is a crate whose procedural macro reads a file Cargo knows nothing about. When Cargo has finished, Jato checks that every file it left out is there again. Were one missing, it would take out of the target directory everything it had put there and have the build made from nothing.

The finished build is recorded like one made from nothing, and the next checkout of those sources gets it whole. The exception is a build in which a build script was itself built again and ran in the directory its earlier version had written to: Cargo is content with what that leaves, but it need not be what a build from nothing leaves, so it is not put back anywhere else.

Manifests are Cargo's own to read, and it reads them again every time it runs. Where anything that only Cargo examined differs, a `Cargo.toml` or a `Cargo.lock`, the entries of a directory where it finds packages or programs, a source file it asked about that has gone, the build starts from the earlier one all the same: its files are put in place, and Cargo plans the build again. What follows from a manifest for a compilation is part of what Cargo remembers of it, so it runs the compilations for which that changed and those that are new, each answered from the cache where it can be. Cargo's settings are the exception, `.cargo/config.toml` and `rust-toolchain.toml`: they reach compilers in ways Cargo does not remember, and a change to them leaves nothing to start from.

A build script is not one of Cargo's compilers, and Cargo runs it again only for what the script told it to watch, going by file times. The record of a build therefore says for each build script what it examined and what it wrote, with the compilers it started counted as its doing. Where something a build script examined differs, a file it reads, a header of C it compiles, the lock file, the script has to run again, and it does: the compiler that made the script's program is left out with what the script wrote, Cargo makes the program again, which is one answer from the cache, and runs it. Should Cargo not run it, everything is taken away and the build made from nothing.

What is no longer part of the build goes. Cargo asks about what each compilation of a build wrote before it decides anything, so what a compilation wrote that it never asked about is taken out of the target directory: a library under its earlier version, a program that was removed. So is what a build script wrote whose program went that way, what a build script that ran again had written and did not write this time, and what Cargo kept for itself in a directory it no longer looks at. Whatever the earlier build wrote outside the target directory, a lock file for one, is neither put back nor removed.

Tried on Olo's own history, each in a new worktree, against the check of the newest commit: a commit back, with the lock file and a manifest changed, 3.1 s, where Cargo carrying the build out took 10; five commits back 9.4 s with 88 compilers run; forty commits back, 181 inputs differing and two build scripts to run again, 20 s with 255 compilers run. After each, every compilation asked for one at a time was answered by a record that matches, and the target directory held file for file what a build from nothing leaves.

Nothing is left to start from where Cargo's settings differ, or what a program examined that is neither Cargo nor part of what a build script did. Cargo then carries the build out, with every compiler answered from the cache. `JATO_NEAR=0` has it carry out every build that is not recorded.

### Build scripts and the C, C++ and assembly they compile

Jato caches Cargo build-script executions directly (`script_hit`/`script_miss`) via a minimal zero-symbol preload library (`libjato_script.so`, separate from the read-note library so it works even with `JATO_NOTES=0`). Each build script is keyed by its binary's BLAKE3 digest, arguments, working directory, and normalized environment, verified against every file, directory, symlink, subprocess, and nested C/Rust tool compilation observed while it ran. Passing, hermetic scripts whose surviving writes stay inside `OUT_DIR` replay deterministically across clean builds, worktrees, and config changes—eliminating downstream recompilation cascades from non-deterministic code generators—while relocating `OUT_DIR`/`WORK` paths in stdout directives and text outputs and preserving exact whole-build attribution. `JATO_CACHE_SCRIPTS=0` disables build-script caching.

A build script that compiles C starts the compiler itself, and Cargo gives nobody a say in that. For the Cargo commands Jato runs, a directory at the head of `PATH` holds the names `cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`, `nasm` and `yasm`, each leading back to Jato, for those of them the rest of `PATH` finds. Started by a build script under one of these names, Jato finds the program itself and treats its run like a run of rustc: looked up, and otherwise run under observation and recorded, with every header it opened or looked for among what the result depends on. gcc and clang are also told the workspace's and the target directory's other names, through `-ffile-prefix-map`, so that debugging information compiled from a file in the workspace does not tie the build to one checkout.

Only a C compiler's compilations are answered this way, those with `-c` or `-S`. Its version, its kind and whatever else a build script asks it are the compiler's to answer each time, and a link is the linker's. A compilation that fails, as an answer to whether a header is installed, is run again every time, and what the compiler looked for is something the build as a whole depends on. A compiler that is sent its source through a pipe is run as it is. Started by anything other than a build script, such as the program `jato run` runs or a test, the names lead straight to the programs. `JATO_TOOLS=0` leaves the directory out.

On the whole Olo workspace this was most of what a second checkout cost. Of 630 processor-seconds for `check --workspace` in a copy of a checkout that had been built, with every Rust compilation restored, 545 went to C, C++ and assembly that 113 build scripts compiled again. With those remembered, the same command in a copy with one crate edited took 65.

### Other checkouts and target directories

Results are stored relative to the workspace and the target directory, so a second checkout or a new target directory finds them. For that to be true of the compiled bytes, Jato asks rustc to write `/proc/self/cwd` in place of the workspace's location and `/proc/self/cwd/target` in place of the target directory's, through `--remap-path-prefix`. A debugger started in the workspace opens the right sources through those names. Diagnostics, `file!()` and panic messages are unchanged.

Text inside the target directory that names the workspace or the target directory is known by what it says without those names, to a compiler that reads it as to the build script that wrote it: source generated with full paths in it is the same source in every checkout, and the crate that includes it is compiled once.

A compiler that a build script starts is the same command whether the build is for `check` or for `build`, except in one thing: Cargo lists `target/debug` among the directories the loader looks through for libraries in a build, and not in a check. Which libraries were loaded is among what the compiler examined, so the list itself is no part of which command it is. A result recorded with one list is used with another when the loader would find the same along both: the directories they share in the same order, nothing found in a directory that has gone, and none of the names that were looked up in a directory that is new. On Olo that is 1,113 commands, most of the C and assembly in the workspace, which a `build` in a new worktree now takes from an earlier `check`.

Output that still names where it was built, such as a program that embeds `env!("CARGO_MANIFEST_DIR")`, is reused only in that checkout. A whole build with such a program in it is put back whole only there, and anywhere else a build starts from it: the record says which compilers wrote output that names the place, everything else is put in the target directory, and Cargo runs those compilers. One of Olo's 56 programs names its checkout. The first `build --workspace` in a new worktree had every compiler answered one at a time for it, 47 s on a 32-processor server, and now takes 9: 4 s to put 21,600 files in place, 2 s to compile one program, 1 s to record the build. `JATO_REMAP=0` keeps real paths in everything, and results then stay with the directory they were made in.

A build can also depend on what is around its checkout. A crate named by `path = "../shared"`, or anything else a command reads from near the workspace without the environment naming the place, is recorded twice: where it was, and by the way there from the workspace. A result is reused only where both are as they were, so a checkout with a different neighbour gets a build of its own and one with the same neighbour shares it. Cargo, rustup and git look for settings in every directory above the workspace. What such a search found, apart from Cargo's and rustup's own directories, has to be what a search from the other checkout finds: a directory with a `.cargo/config.toml` above one checkout and not the other keeps their builds apart. A build that writes files beside its workspace is reused only there. What programs keep for a person in the home directory under a name that begins with a dot, as git keeps `.gitconfig`, they find by the home directory's name, and it is recorded where it is and no second time: a checkout directly in the home directory and one three levels down share their builds.

Cargo asks git which files a package has when the package's build script names no inputs, to compare their times with what it built. A repository's own files differ between a checkout, a worktree of it and a clone, and they change with every commit. What Cargo itself reads inside a repository's directory is therefore not counted among a build's inputs: a build made in one worktree is put back in another, on another branch, and after a commit that left the built files alone. What any other program reads there counts like everything else, so a build script that puts the current commit into the program gets a build for each commit. The directories a workspace is inside are not inputs either, and a build outlives the checkout that made it.

On its way to a package's files Cargo lists the top of the repository and every directory of the package. It goes by the names in a directory only where it looks for a package's programs, tests, examples and benchmarks, and where a workspace finds its packages by a pattern such as `crates/*`. What it lists elsewhere is not counted: a file git does not track, an editor's leftovers, or the `target` directory of an earlier build beside a build made into another, keep no build from being put back. A directory that a build script or a compiler lists counts in full.

The files of a build that is put back all get one time, taken just before Jato looks at the sources, so that no source looks newer than the build and any file edited from then on does, one saved while the files are still being written included. A file outside the target directory that already holds what the build wrote, such as `Cargo.lock`, is left as it is.

### Edits

When you edit sources in an already-built target directory (outside a rapid editing spell and while the machine is not overloaded), Jato verifies unchanged outputs in parallel, removes only the stale outputs (leaving `rustc`'s `incremental/` state untouched), and runs Cargo under watch so the edited state is recorded as a whole build (`build_continued`) while also catching hidden inputs Cargo would miss (such as files read only by procedural macros or edits with older timestamps). Another checkout or worktree with that edited state then gets an instant whole-build hit. `JATO_RECORD_EDITS=0` skips in-place whole-build recording.

A crate compiled again within ten minutes of its last compilation in the same target directory is being edited. rustc compiles it incrementally from the state it saved, Jato looks for a recorded result (which is how undoing an edit is instant) and otherwise leaves the compiler unwatched and its output unrecorded, because the next edit replaces it. The first compilation after a pause is recorded again. `JATO_EDIT_WINDOW` sets the pause in seconds; `0` records every compilation.

Watching a compiler is free while processors are free. On a machine where every one is in use, each file the compiler asks about costs it a turn at the processor, and a short compilation can take several times as long: checking `olo-system` and its 54 dependents after an edit took 17 to 25 s watched and 7 to 14 s unwatched at a load of 45 to 55 on 15 processors. So in a target directory that is already built, compilations are left unwatched while the load average is at or above the number of processors, and recorded when it is below. A build made from nothing is always recorded. `JATO_BUSY` sets the load; `0` always watches.

A target directory that was restored holds none of the state rustc saves for compiling a crate again, so the first edit of a crate there would compile the whole crate. Jato keeps that state in the cache, once for each crate and replaced by newer state when it is more than an hour old, and hands it to a compilation that has none of its own: the first edit in a new checkout is compiled incrementally, from state saved where the crate was last compiled under watch. On Olo that took the first edit of the top crate in a new checkout from about 110 processor-seconds to 53.

What rustc makes of the state depends on the crate. For a library it reuses its analysis and the code it generated for every module that was not edited. For a program or a test in another target directory it reuses the analysis and generates all code again: rustc ties the code of anything it links to the real locations of the libraries linked in, whatever names they are given by. In the target directory the state was saved in, after a clean, a program's code is reused too.

rustc accepts saved state only under the options it was made with, and the workspace's location is one of them. Jato sets the few bytes at the start of the state that stand for the options to this checkout's, which it learns by having the compiler compile a one-line crate, and names the output directory through `/proc/self/cwd` so that it reads the same everywhere. Where unprivileged Linux user and mount namespaces are permitted by the kernel, Jato can also bind-mount the target directory at a canonical path inside a private mount namespace (`JATO_MOUNT=0` disables it) so `rustc` sees identical `.rlib` and `--out-dir` paths across checkouts and out-of-tree target directories, falling back automatically when user namespaces are restricted. rustc still checks each saved result against the sources before using it, and if the state does not begin the way Jato expects it is left out and the crate is compiled from scratch. State is kept for the output directory it was made with, since rustc generates all code again for any other. A target directory inside the workspace reads the same in every checkout. One kept anywhere else, such as a scratch `CARGO_TARGET_DIR` for each checkout, is held open for the compiler on a descriptor and named through that, `/proc/self/fd/219`, when the crate is a library, which rustc writes itself. Programs, tests and procedural macros there are compiled without state the first time: a linker would have to keep the descriptor. `JATO_SAVED_STATE=0` turns this off.

### Builds at the same time

Every compiler started through Jato takes one of as many places as the machine has processors, shared by all builds that use the same cache. Five builds started together then run fifteen compilers on a fifteen-processor machine, not seventy-five. A place is a file lock, so it is given up however its holder ends. `JATO_JOBS` sets the number of places; `0` removes the limit.

Two builds that need the same compilation at the same moment run it once: the second waits for the first and uses its result. `JATO_COALESCE=0` lets them run independently.

Cargo shares out a token for every compiler it runs at once, through a pipe the compilers inherit. To hand on a pipe that has no name it starts each compiler as a copy of itself, with all the memory it holds for the workspace, which then becomes the compiler. Where Jato has an answer ready for every compiler that copying is most of what is left: for a check of all of Olo with 2,618 compilations answered and 49 run, 29 of 60 processor-seconds, and it is done one compiler at a time. Jato gives Cargo its tokens in a shared machine-wide named FIFO (`tokens-shared` in the cache directory) coordinated by file locks across all concurrent builds sharing the cache, so Cargo, `rustc -Zthreads`, LLVM codegen threads, and GNU `make` share a single global token pool and clean up automatically when the last build exits. Cargo then starts a compiler without a copy of itself, and that check took 11 s where it had taken 15. The number of tokens is the one Cargo would have chosen (or `JATO_JOBS`). Where anything says how many compilers to run, `-j`, `CARGO_BUILD_JOBS`, a `jobs` setting or tokens handed down by make, the arrangement is left as it is. So it is where a build script would find a make older than 4.4, which stops at a pipe with a name. `JATO_JOBSERVER=0` leaves Cargo its own pipe always.

### Shared and remote caches

Set `JATO_REMOTE` or persist a remote URL with `jato remote <URL>` to share verified whole builds, single compilations, C tool outputs, build-script runs, and test results across machines or CI runners with zero daemon and zero extra dependencies:

- `file:///path/to/shared/dir` (or `/path/to/shared/dir`) for a shared directory or NFS mount
- `http://host[:port][/prefix]` for any HTTP/1.1 PUT/GET/HEAD CAS server (with optional `JATO_REMOTE_TOKEN` Bearer auth)

On a local miss, Jato fetches remote candidate manifests, cryptographically verifies the manifest and detail digests, checks local inputs *before* downloading any output objects, downloads missing zstd-compressed objects in parallel with streaming BLAKE3 verification, and adopts them transactionally into the local store. `JATO_REMOTE_READONLY=1` pulls without pushing. Transport errors or corrupted remote blobs never fail a build.

## Use with other tools

To reuse single compilations under plain Cargo or a tool that starts Cargo, name Jato as the compiler wrapper for that process, or in `.cargo/config.toml`.

```sh
RUSTC_WRAPPER="$(command -v jato)" cargo build --release
```

```toml
[build]
rustc-wrapper = "jato"
```

Whole builds are recorded and restored only by Jato's own commands, which is the faster way for anything large. A workspace wrapper such as Clippy's keeps working beneath Jato. Cargo runs build scripts and test programs itself.

### Alongside another compiler cache

A compiler that runs behind kache, ccache or sccache does its work where Jato cannot watch it, so nothing it takes part in could be recorded. Jato therefore starts compilers directly:

- `RUSTC_WRAPPER` is Jato for the Cargo commands Jato runs, whatever Cargo's configuration names.
- A directory on `PATH` that holds nothing but links to such a tool under compilers' names is left out for everything Jato starts.
- A C compiler setting of the form `kache cc`, in `CC`, `CXX`, `HOST_CC` and their relatives or in a plain `[env]` entry of Cargo's configuration, is passed to Cargo as `cc`.
- rustdoc or rustc named by a path that is a link to such a tool, in `RUSTDOC`, `RUSTC` or under `[build]` in Cargo's configuration, is passed to Cargo by its own name. Cargo tells build scripts where both are, and some ask them for their version.

Nothing is changed outside the build Jato runs, and `JATO_DISABLE=1` turns all of it off. If a build still reaches another cache, `jato stats` says that the build was not recorded, which program it was, and the socket or file it used.

## Work on save

| Command | Behavior |
| --- | --- |
| `jato watch` | Check the workspace. After a member is edited, check its package and then the workspace, including dependents and unified features. |
| `jato watch --focus-only` | Give feedback on the edited members. The first run and shared configuration changes still check the workspace. |
| `jato watch -- test --workspace` | Run these Cargo arguments on every save. |
| `jato watch --watch ../schemas -- check --all-targets` | Watch an additional file or directory. |
| `jato watch --debounce 40` | Use a fixed 40 ms quiet window. By default a completed save of one Rust file starts a check at once; other small edits get 16 ms and bursts or configuration changes 100 ms. |

The watcher uses native filesystem events and Cargo metadata, and routes each job through `jato` itself so saves benefit from whole-build hits on undo, in-place whole-build recording, cross-command seeding, build-script caching, and observed test caching. It follows workspace directories, member directories and direct local path dependencies, honors ignore rules, and leaves out Cargo's outputs and Jato's cache. A new save supersedes a running job, including its compiler and test processes.

`jato dev build`, `run`, `test` and `watch` select the Cargo profile `jato-dev`, which inherits the project's `dev` profile, uses 16 codegen units, keeps line tables for workspace members and omits debug information for dependencies. Its outputs go to `target/jato-dev`. An explicit `--profile` or `--release` takes precedence.

## Settings

| Setting or command | Purpose |
| --- | --- |
| `JATO_CACHE_DIR` | Private directory on a local filesystem. Defaults to `$XDG_CACHE_HOME/jato`, or `~/.cache/jato`. Builds that share it share results and processors. |
| `jato limit 32GiB` | Set the budget for this cache directory. Without one it is a tenth of the filesystem, at least 10 GiB and at most 100 GiB. `jato limit` prints it; `--unset` removes it. |
| `jato memory 64GiB` | Set a RAM budget for accelerating builds on high-memory machines: scales SQLite `cache_size`/`mmap_size` and stores hot objects uncompressed on non-cloning filesystems for single-pass SIMD BLAKE3 restores at memory bandwidth. `jato memory` prints it; `--unset` turns it off. |
| `jato remote <URL>` | Configure a shared/remote cache (`file:///path` or `http://host[:port][/prefix]`). `jato remote` prints it; `--unset` clears it. |
| `JATO_REMOTE` | Remote cache URL for this process (overrides `jato remote`; `0` disables). |
| `JATO_REMOTE_READONLY=1` | Pull from the remote cache without pushing local results. |
| `JATO_REMOTE_TOKEN` | Bearer token sent in `Authorization: Bearer <token>` for HTTP remotes. |
| `JATO_MEMORY=64GiB` | Memory budget for this process (overrides `jato memory`). |
| `JATO_MAX_SIZE=4GiB` | Budget for this process when none is stored with `jato limit`. |
| `jato gc 10GiB` | Evict older low-value results down to this size. `jato gc` uses the budget; `jato gc 0` empties the cache. It also removes what earlier versions of Jato left in the directory. |
| `jato stats [--json]` | Builds put back whole, started near, seeded, or continued in-place; Rust, C/asm, build-script, and test results reused, run, left to rustc during edits or not storable; remote pulls/pushes; disk and hot memory tier use. |
| `jato why` | Print why the last build was not recorded or was pinned to its target directory, plus filesystem/memory tips. |
| `JATO_LOG=debug` | Say for each command whether it was reused and, if not, what had changed, and for a build put back how long each step took. An absolute path appends to that file, with times. |
| `JATO_RECIPES=/dir` | Write what each command is known by into a file of its own in this directory, a line for each thing, with the workspace and the target directory as marks. Two runs' directories compared show why a command of one was not found for the other. |
| `JATO_KEY_ENV=A,B` | Variables a procedural macro reads from the environment. |
| `JATO_JOBS=8` | Compilers (or parallel test binaries) running at once across the builds sharing this cache. |
| `JATO_EDIT_WINDOW=0` | Record every compilation, including each edit. |
| `JATO_BUSY=8` | The load average from which compilations in a built target are left unwatched. Defaults to the number of processors; `0` always watches. |
| `JATO_WHOLE_BUILDS=0` | Reuse single compilations only. |
| `JATO_NEAR=0` | Never start a build from a recorded one whose sources differ. |
| `JATO_RECORD_EDITS=0` | Skip in-place whole-build recording after edits in an already-built target. |
| `JATO_SEED=0` | Never seed an empty target directory from another command's build. |
| `JATO_CACHE_SCRIPTS=0` | Run build scripts without caching their executions. |
| `JATO_CACHE_TESTS=0` | Run `jato test` without caching test results. |
| `JATO_PARALLEL_TESTS=0` | Run workspace test binaries one at a time. |
| `JATO_ASYNC_RECORD=0` | Wait for linked compiler output staging before waking Cargo. |
| `JATO_JOBSERVER=0` | Leave Cargo to make the pipe its compilers share tokens through. `1` gives it one with a name without asking `make` its version. |
| `JATO_TOOLS=0` | Leave the C compilers and assemblers that build scripts start to themselves. |
| `JATO_SAVED_STATE=0` | Do not keep rustc's incremental state in the cache or hand it to other checkouts. |
| `JATO_MOUNT=0` | Disable optional unprivileged mount-namespace target canonicalization. |
| `JATO_NOTES=0` | Have the kernel report every file call of a watched program; none are told of by the program itself. |
| `JATO_STORAGE=packed` | Keep compressed copies even where files can be cloned. |
| `JATO_VERIFY=1` | Check the digest of restored files on a filesystem that checksums them. |
| `JATO_DISABLE=1` | Run Cargo and the compiler directly. |

## Storage

The cache directory holds an SQLite index and the files commands wrote, each stored once under the BLAKE3 digest of its contents. On a filesystem that can clone files, such as Btrfs or XFS, a stored file is a clone of the compiler's output and a restored file is a clone of that: the bytes exist once on disk however many target directories hold them, and each copy can still be written without affecting the others. On other filesystems objects are compressed with Zstandard and restores are copies. A file is read before it is compressed, and one the cache already holds is not compressed again: recording a build of all of Olo's programs on ext4, 13 GB most of which each compiler had stored as it finished, took 20 s after Cargo was done and now takes 2.5.

Measured on Btrfs, for builds and tests of two programs of Olo: a cache of 807 MiB and four target directories filled from it, 2,651 MiB by their sizes, held 22 MiB between them that only one of the five had. Each worktree's target directory had 0 to 3 MiB of its own. One that bare Cargo built beside them had all 153 MiB of its own.

A result becomes visible in one transaction, after its files are in place. Each record says itself which command it belongs to and which files it needs; the index only finds records and counts references. A restored file's digest is checked unless the filesystem checksums data itself. A record that cannot be read or whose files are gone is dropped and the command runs.

Next to results the cache keeps the state rustc saved for each crate of a workspace, which is about as large as the crate's compiled output. It is stored once however many checkouts are handed it.

### On a build server

Measured on a server with 32 processors (AMD EPYC 9655), 125 GiB of memory and a disk that writes 7.8 GB/s, for Olo's whole workspace (121 packages, 56 programs). On ext4 with Jato 0.19.0 (`jato memory 64GiB` in parentheses), vs Btrfs reflinks (measured with 0.16.0):

| | ext4 (`0.19.0`) | Btrfs (`0.16.0`) |
| --- | ---: | ---: |
| `check` from nothing | 86.7 s | 79 s |
| `check` in a new worktree | 0.38 s | 1.0 to 1.3 s |
| `check` in a new worktree, a function added to a crate 54 others use | 3.5 s | |
| `check` in a new worktree, a manifest changed | 3.0 s | |
| `build` from nothing | 246 to 257 s | 200 s (after check) |
| `build` put back, 13,912 files and 20.9 GiB | 1.25 s (0.73 s) | 1.3 s |
| `build` in a new worktree, one program compiled again for where it is | 6.4 s (5.6 s) | 6.5 to 7 s |
| `build` in a new worktree, the function added | 32 s | 45 s |
| Room for the cache and three target directories | 8.6 GiB + 21 GiB each (hardlinked pairs) | 33 GiB in all |

Cargo alone takes 76 s for `check` from nothing (whether its target directory is on disk or in memory) and 248 s for `build` from nothing.

Beside kache 2.3.0 on that server, the two started together (`bench/README.md` has every step): in 0.18.0 Jato matched kache on cold builds (154 vs 161 s `check`, 480 vs 482 s `build`), beat kache by 8–27× on restores and new targets, and beat kache by 2× on manifest changes (`10.1 s` vs `19.6 s` on `build`) once incremental `.rlib` session normalization eliminated downstream relink cascades, using half the disk space (6.2 GiB vs 12.3 GiB) and zero idle daemon RAM. 0.19.0 cut Jato's 8 cache-served steps by another 40% (including a 13–15× faster `undo` at 0.69 s).

Put the cache and the checkouts on a filesystem that clones files where one is to be had. Nothing is copied when a build is put back there, and a target directory takes no room that the cache does not already take. `jato stats` says whether the cache's filesystem clones. A file on any filesystem can hold one:

```sh
truncate -s 300G /srv/builds.img && mkfs.btrfs /srv/builds.img
mkdir -p /srv/builds && mount -o loop,noatime /srv/builds.img /srv/builds
```

Memory acceleration (`jato memory 64GiB` / `JATO_MEMORY=64GiB`): on high-RAM build servers with non-cloning filesystems like ext4, configuring a memory budget scales SQLite's page cache and mmap window and keeps hot objects up to the budget uncompressed in the cache directory so they stay resident in the Linux kernel page cache. Staging skips zstd compression in a single pass, and restores stream single-pass read + SIMD BLAKE3 verification + write without rewinding, hardlink-deduplicating identical large `TARGET` binaries (such as Cargo's `deps/<bin>-<hash>` and `<bin>` pairs) and atomically renaming fresh target directories into place. On the 32-core EPYC server with `jato memory 64GiB` on ext4 (`0.19.0`), Olo's `fresh` checked 72,527 inputs in **0.073 s**, a full **20.9 GiB** verified restore into a new target finished in **0.73 s (28.7 GB/s effective)**, and `undo` of 11.8 GiB of changed binaries took **0.74 s** (down from 8.94 s in `0.18.0`).

What a machine with many processors gains is in how a build is verified, put back, recorded, and tested: manifest input checks run on up to 24 lock-free threads with zero per-input path allocations and lazy manifest tail decoding, restores run on up to 32 threads with hardlink deduplication, atomic fresh-target renames, and ext4 `auto_da_alloc` overwrite avoidance, linked compiler outputs record asynchronously behind a bounded pool that expands to all cores once Cargo exits, and workspace test binaries execute in parallel.

The budget of a cache nobody set one for is a tenth of its filesystem, at least 10 GiB and at most 100 GiB. A debug build of a large workspace can be larger than a small filesystem's tenth: Olo's is 21 GiB. Such a build is not recorded, `jato stats` says so, and `jato limit` sets a budget that holds it.

There is no daemon, and nothing runs between builds. The cache holds compiled artifacts and diagnostics, which may include values embedded by `env!`.

## What is not reused

- A command that failed. Its diagnostics and exit status are the compiler's own every time.
- A command that used the network, talked to a service through a socket Jato does not know to be harmless, or opened files in a way that cannot be watched.
- A command that changed something it had read, other than the outputs compilers and linkers rewrite in place.
- A compilation whose source arrives on standard input.

`jato stats` counts these as not storable, and `JATO_LOG=debug` says why, naming the program and the file or socket where there is one.

What leaves no trace in files or in the variables above is not seen at all: the time of day, a random number, the state of a machine across the network. A C file that asks for the time it is compiled at, through `__TIME__` or `__DATE__`, is given back with the time of its first compilation, as a Rust crate whose procedural macro reads the clock is.

## Comparing Jato, kache, sccache, and Bazel (Blaze)

These four tools sit at different points between a compiler wrapper and a full build system. **kache** and **sccache** wrap individual compiler calls under Cargo. **Bazel** (the open-source counterpart to Google's internal **Blaze**) replaces Cargo with a declarative polyglot build graph and optional Remote Build Execution (RBE). **Jato** keeps Cargo in charge, but observes real Linux system calls to cache whole builds, near-build deltas, build scripts, C/C++/asm, `rustc` incremental state, and test runs without a build description of its own.

### What each tool does

| | Jato | kache | sccache | Bazel / Blaze |
| --- | --- | --- | --- | --- |
| **Build system** | Keeps Cargo (`jato build`/`test`/`check`, or `RUSTC_WRAPPER=jato`) | Keeps Cargo (`RUSTC_WRAPPER` + C/C++/CUDA wrapper) | Keeps Cargo (`RUSTC_WRAPPER` + C/C++/CUDA wrapper) | Replaces Cargo (`BUILD.bazel`, `rules_rust`, `crate_universe`) |
| **How inputs are learned** | Observed at runtime via Linux `seccomp_unotify` + read-note shim (exact files, dirs, symlinks, child processes) | Parsed from normalized compiler flags and compiler-reported inputs | Parsed from compiler flags, preprocessor/dep-info, and env | Explicitly declared in `BUILD` files and enforced by hermetic sandboxes |
| **Unit of caching** | Whole Cargo builds (bypasses Cargo on hit), near-build deltas, cross-command target seeding, single `rustc`/C/asm units, build scripts, tests | Single compiler invocations (`rustc`, C/C++, CUDA); Cargo still plans and runs every step | Single non-incremental compiler invocations (`rustc`, C/C++, CUDA) | Every action in a polyglot graph (Rust, C++, Go, Java, Python, codegen, tests) |
| **Proc macros & hidden file reads** | Observed automatically; unlisted env vars via `JATO_KEY_ENV` | Relies on `rustc` dep-info; untracked reads can be missed | Relies on `rustc` dep-info; untracked reads can be missed | Enforced by sandbox: undeclared inputs fail or are invisible |
| **Build scripts (`build.rs`) & C/C++** | Caches both `build.rs` executions (`OUT_DIR` + directives) and nested C/C++/asm compilations | Caches C/C++/CUDA via `CC`/`CXX` wrappers; Cargo still runs `build.rs` | Caches C/C++/CUDA via `CC`/`CXX` wrappers; Cargo still runs `build.rs` | First-class sandboxed `cargo_build_script` and C/C++ actions |
| **Incremental `rustc` state** | Kept and handed on across cleans and worktrees (reuses analysis + unedited library codegen on first edit) | Compiles without `rustc` incremental state (first edit in a restored target compiles crate from scratch) | Skips caching incremental Rust crates (dev-profile workspace crates miss unless `CARGO_INCREMENTAL=0`) | Non-incremental `rustc` actions by default (mitigated by fine-grained targets + RBE) |
| **Test caching & execution** | Observed caching of passing hermetic test binaries + parallel workspace test runner | None (Cargo runs tests) | None (Cargo runs tests) | First-class sandboxed test caching, sharding, and remote execution |
| **Daemon & idle RAM** | None (`0 MiB` idle); optional shared named-FIFO jobserver | Background daemon (`21–115 MiB` RSS measured) for async remote sync & concurrency limiting | Local client-server daemon | Persistent JVM server per workspace (hundreds of MiB to multi-GiB on large graphs) |
| **Local storage & dedup** | SQLite + BLAKE3 CAS; Btrfs/XFS reflinks, ext4 zstd + hardlink dedup + optional RAM hot tier (`jato memory`) | SQLite + BLAKE3 CAS; APFS/Btrfs/XFS reflinks or hardlinks/copies | Flat local disk cache (no reflink target deduplication) | Output base + content-addressed disk cache |
| **Remote cache & RBE** | Built-in zero-dep `file://` and HTTP/1.1 CAS (`jato remote`); no S3/GCS SDK or RBE | Native S3-compatible remote cache (S3/R2/MinIO) + GitHub Actions; no RBE | S3, GCS, Azure, Redis, Memcached, WebDAV, GHA + `sccache-dist` distributed builds | Industry-standard REAPI: remote caching + Remote Build Execution (RBE) worker farms |
| **Platforms** | Linux (`x86_64`, `aarch64`, kernel 5.5+); passthrough to Cargo on macOS/Windows | Linux, macOS, Windows | Linux, macOS, Windows | Linux, macOS, Windows (strongest sandboxing on Linux) |

### Relative performance

Measured numbers below come from [bench/README.md](bench/README.md), where each contender gets its own store, daemon, checkout, and target directories and the built binary is verified after every step:

| Scenario | Jato | kache `2.3.0` | sccache `0.19.0` | Bare Cargo | Notes |
| --- | ---: | ---: | ---: | ---: | --- |
| **Large workspace (Olo `build --workspace`)** — cold¹ | 480.4 s *(246 s solo)* | 482.3 s | — | *(248 s solo)* | Cold parity (~1% CPU overhead) |
| Olo `build` — warm repeat (`fresh`, 72k inputs)¹ | **0.07–0.15 s** | 1.84 s | — | — | Jato skips Cargo (12–26× faster) |
| Olo `build` — target removed / new target¹ | **0.73–3.06 s** | 30.1–138.7 s | — | cold | 10–27× faster (`0.73 s` with `jato memory 64GiB`) |
| Olo `build` — `Cargo.toml` edit in new worktree¹ | **7.6–10.1 s** | 19.6 s | — | cold | 2× faster; near-build + `.rlib` normalization |
| Olo `build` — first edit in restored target¹ | **10.6–18.2 s** | 36.0 s | — | — | 2–3.4× faster; handed-on `rustc` saved state |
| Olo `build` — subsequent edits (in-place)¹ | 8.7–13.1 s | 11.7 s | — | — | Parity (Cargo + `rustc` incremental dominate) |
| Olo `build` — undo edits¹ | **0.69 s** *(0.19)* | 11.4 s | — | — | Whole-build restore (`16×` faster in `0.19.0`) |
| Olo `check` — target removed / new worktree¹ | **0.37–1.52 s** | 10.4–11.9 s | — | cold | 8–27× faster; store `6.2` vs `12.3 GiB` (`build`) |
| **Small workspace (`ripgrep 14.1.1` dev)** — restore / copy² | **0.07–0.34 s** | 0.38–0.70 s | 4.86–8.09 s | 9.12–11.32 s | sccache skips incremental dev workspace crates |
| `ripgrep 14.1.1` dev — subsequent edits² | 0.52 s | 0.53 s | 0.47 s | 0.50 s | Parity across all tools |
| `ripgrep 14.1.1` dev — undo² | **0.05 s** | 0.16 s | 0.51 s | 0.69 s | Whole-build restore |

¹ Olo (121 packages, 1,502 Rust compilations, 113 build scripts / ~1,100 C/C++/asm files, 56 dev binaries, 20.6–20.9 GiB target, `nightly-2026-11-14`, mold) on a 32-core AMD EPYC 9655, 125 GiB RAM, ext4. Side-by-side runs (`0.18.0` vs `kache 2.3.0`, load 7–67, 1 run each) plus `0.19.0` single-contender runs on the same server (load 15–49).  
² `ripgrep 14.1.1` (default dev profile, Rust 1.100) on an Intel Core Ultra 7 356H, Btrfs, median of 3 rotating runs, load 21–30.

**Where Bazel / Blaze sits on performance** (not benchmarked in `bench/compare.py`, since migrating a Cargo workspace to `BUILD.bazel` changes the build graph itself):
- **Cold builds at scale**: With **Remote Build Execution (RBE)**, Bazel/Blaze can fan hundreds of compile and test actions across a remote worker fleet and beat any single-machine tool (Jato, kache, or sccache) on cold builds.
- **Warm & cross-branch builds**: Cached action hits and selective test re-runs are fast once the Bazel JVM server is hot, though cold-server startup and graph analysis add latency and gigabytes of resident memory that daemonless Jato avoids.
- **Inner edit loop on a single large crate**: Because Bazel normally runs non-incremental `rustc` actions, editing a large crate locally is typically slower than Cargo/Jato's `rustc` incremental compilation unless the codebase is broken into many small targets.

### When each tool is the right fit (and when it is not)

- **Jato**
  - *Right fit*: Linux workstations, shared Linux build servers, and multi-worktree coding agents (Claude Code, Codex, Antigravity) on standard Cargo workspaces—especially large workspaces with slow build scripts, C/C++ dependencies, proc macros, or frequent branch/worktree switches where whole-build restores, handed-on incremental state, observed soundness, test caching, and zero-daemon concurrency control pay off most.
  - *Not the right fit*: macOS or Windows build hosts (Jato passes through to Cargo there); teams that need native S3/GCS/Azure object-store caching out of the box or RBE worker farms; polyglot non-Cargo monorepos.
- **kache**
  - *Right fit*: Cross-platform (Linux, macOS, Windows) Rust + C/C++/CUDA teams wanting a fast, modern drop-in `RUSTC_WRAPPER` with reflinks, BLAKE3 keys, and native S3/R2/MinIO + GitHub Actions remote caching without leaving Cargo.
  - *Not the right fit*: Workspaces where Cargo startup, `build.rs` execution, hidden proc-macro inputs, or cold first-edit compiles in fresh worktrees dominate—kache still runs Cargo and build scripts on every restore and compiles without `rustc` incremental state.
- **sccache**
  - *Right fit*: Cross-platform CI pipelines (especially with `CARGO_INCREMENTAL=0`) and mixed Rust/C/C++/CUDA teams that want battle-tested cloud storage backends (S3, GCS, Azure, Redis, Memcached, GHA) or `sccache-dist` distributed compilation.
  - *Not the right fit*: Local default-profile (`dev`) Cargo development and multi-worktree workflows—sccache does not cache incremental Rust crates, does not bypass Cargo on restores, and does not cache build scripts or tests.
- **Bazel / Blaze**
  - *Right fit*: Large polyglot monorepos (Rust + C++ + Go + Protobuf + Python/Java) that require strict hermetic reproducibility, unified cross-language graphs, fine-grained test sharding, and Remote Build Execution (RBE) across clusters.
  - *Not the right fit*: Cargo-first Rust teams that want zero migration effort, out-of-the-box `Cargo.toml` / `rust-analyzer` / crates.io ergonomics, low idle memory use, and `rustc` incremental compilation without maintaining `BUILD.bazel` files and rule generators.

## Design references

- BuildXL and Bazel record what an action read and key its result on that. Jato applies the two-step lookup to Cargo without a build description of its own: Cargo stays in charge, and the observation supplies the dependency list. [BuildXL two-phase lookup](https://github.com/microsoft/BuildXL/blob/main/Documentation/Wiki/Advanced-Features/Two-Phase-Cache-Lookup.md) · [Bazel remote caching](https://bazel.build/remote/caching)
- ccache's direct mode checks a manifest of the files a compilation included before running the preprocessor. Jato's manifests are the same idea with the list taken from the kernel. [ccache manual](https://ccache.dev/manual/latest.html)
- seccomp user notification lets a supervisor see a system call before it happens and let it proceed. A filter can also tell where in a program a call was made from, which is what lets a library in the program vouch for the calls it makes itself. [seccomp_unotify(2)](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html) · [seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html)
- Reflinks let a restored file share its extents with the cache until someone writes to it. [FICLONE](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html)
- kache and sccache cache single rustc invocations keyed on their arguments and declared inputs. Jato is measured against both. [kache](https://github.com/kunobi-ninja/kache) · [sccache](https://github.com/mozilla/sccache) · [Measurements](bench/README.md)

## Development

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. The integration suite builds disposable Cargo projects and checks restored bytes, invalidation, other checkouts and what is beside and above them, procedural macros, build scripts and the C they compile, linkers, damaged caches and concurrent builds. Run `python3 tests/install_skill.py` to check the skill installer. Project conventions, open work and the journal live alongside the code. [AGENTS.md](AGENTS.md) · [TODO.md](TODO.md) · [JOURNAL.md](JOURNAL.md)

## License

Jato is distributed under the [MIT License](LICENSE).
