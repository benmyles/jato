# Jato

Jato remembers Rust builds. A build that has been made before, with the same sources, settings and toolchain, is put back without starting Cargo: after `cargo clean`, in another target directory, in a git worktree created a moment ago. A build of sources that differ a little from ones built before starts from that build, and Cargo compiles what changed. Builds that do have to compile go at Cargo's speed, reuse every compilation that still applies, the C that build scripts compile included, and take turns at the processors when several are started together.

On Olo's `olo-system`, 279 compilations with the project's pinned nightly, measured against kache 2.3 and bare Cargo on a 15-processor machine that was busy with other builds:

| Seconds | Jato | kache | Cargo |
| --- | ---: | ---: | ---: |
| The same build again | 0.09 | 0.60 | 0.55 |
| Target directory removed | 0.20 | 2.6 | as cold |
| A target directory never used | 0.21 | 3.1 | as cold |
| The sources copied to another path | 0.33 | 6.8 | as cold |
| First edit of the top crate in that copy | 13 | 25 | |
| Later edits | 8.0 | 7.8 | 9.9 |
| The edits undone | 0.12 | 6.3 | 9.0 |

Checking all 121 packages of that workspace in a clone of its repository, with the 1,100 C, C++ and assembly files its build scripts compile. Jato 0.15 and kache ran side by side, at a load of 29 to 38:

| Seconds | Jato | kache |
| --- | ---: | ---: |
| The same check again | 0.83 | 1.37 |
| Target directory removed | 2.2 | 18.0 |
| A target directory never used | 2.5 | 30.3 |
| The sources copied to another path | 3.8 | 13.2 |
| A function added to `olo-system` in that copy | 8.4 | 16.6 |
| Another copy, the function added before anything is built there | 14.8 | 32.8 |
| Further edits in a built target, median | 8.5 | 24.5 |
| The edits undone | 0.92 | 41.8 |
| Nothing stored, nothing built | 914 | 938 |

In a git worktree a first build with nothing changed is put back in 2 s, one with a line changed in one program takes 7 s, and the next worktree with that change 2 s again.

A build from nothing takes the processor time it takes with bare Cargo: 769 processor-seconds against 763 for `olo-system`, 2,319 against kache's 2,297 for the whole workspace. On ripgrep, a small project, the restores took 0.07 to 0.34 s with Jato, 0.38 to 0.70 s with kache and 5 to 8 s with sccache, and a build from nothing 5 to 8% more processor time than bare Cargo's. [All measurements, with conditions and reproduction commands](bench/README.md)

## Get started

Build Jato on Linux with Rust 1.89 or later and a C toolchain. SQLite and Zstandard are bundled, and so is the library watched programs load, which is built with Jato on x86-64. From this repository, run

```sh
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

### One target directory, several commands

A check, a build, clippy and the tests of a workspace share one target directory, and Cargo uses for each what the others made: build scripts and procedural macros are built once, and clippy checks dependencies exactly as `cargo check` does. A build made where another command's recorded build already is, with all of that build's inputs as they were, is recorded as resting on it. The record holds that build's files with its own, under everything either depended on, and is put back whole in a worktree where the other command was never run. For one package of Olo, clippy first in a new worktree took 55 s without this and 0.5 s with it.

### Builds that start from an earlier one

The first build in a new worktree is often of sources nobody has built: a commit that is new, or an edit made before anything was built there. No recorded build stands for it. Cargo then has to carry the build out, with every compiler it starts answered from the cache, and for a workspace of some size that takes a while even when one file differs: checking the 121 packages of Olo in a new worktree took 23 s and 75 processor-seconds with one line changed in one program, and 37 s with a function added to a crate that 54 others use.

Such a build starts from the nearest recorded one instead. The record of a whole build says who examined each of its inputs: Cargo itself, a build script or something a build script started, or one of the compilers Cargo started. If everything that differs was examined by those compilers alone, the earlier build's files are put in the target directory without the files those compilers wrote. Cargo finds them missing, runs those compilers, then whatever uses what they make, and finds the rest up to date. The same two builds took 7.2 s and 11.6 s, at a load of 18 to 25, and the next worktree with either change got its build whole in 2 s.

File times play no part in which compilers run again. A changed file that looks older than the earlier build is compiled, and so is a crate whose procedural macro reads a file Cargo knows nothing about. When Cargo has finished, Jato checks that every file it left out is there again. Were one missing, it would take out of the target directory everything it had put there and have the build made from nothing.

The finished build is recorded like one made from nothing, and the next checkout of those sources gets it whole. The exception is a build in which a build script was itself built again and ran in the directory its earlier version had written to: Cargo is content with what that leaves, but it need not be what a build from nothing leaves, so it is not put back anywhere else.

A change to anything Cargo or a build script read leaves nothing to start from: a manifest, the lock file, a file a build script reads or C it compiles, a new file where Cargo looks for programs to build. Cargo carries such a build out as before. `JATO_NEAR=0` has it carry out every build that is not recorded.

### C, C++ and assembly in build scripts

A build script that compiles C starts the compiler itself, and Cargo gives nobody a say in that. For the Cargo commands Jato runs, a directory at the head of `PATH` holds the names `cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`, `nasm` and `yasm`, each leading back to Jato, for those of them the rest of `PATH` finds. Started by a build script under one of these names, Jato finds the program itself and treats its run like a run of rustc: looked up, and otherwise run under observation and recorded, with every header it opened or looked for among what the result depends on. gcc and clang are also told the workspace's and the target directory's other names, through `-ffile-prefix-map`, so that debugging information compiled from a file in the workspace does not tie the build to one checkout.

Only a C compiler's compilations are answered this way, those with `-c` or `-S`. Its version, its kind and whatever else a build script asks it are the compiler's to answer each time, and a link is the linker's. A compilation that fails, as an answer to whether a header is installed, is run again every time, and what the compiler looked for is something the build as a whole depends on. A compiler that is sent its source through a pipe is run as it is. Started by anything other than a build script, such as the program `jato run` runs or a test, the names lead straight to the programs. `JATO_TOOLS=0` leaves the directory out.

On the whole Olo workspace this was most of what a second checkout cost. Of 630 processor-seconds for `check --workspace` in a copy of a checkout that had been built, with every Rust compilation restored, 545 went to C, C++ and assembly that 113 build scripts compiled again. With those remembered, the same command in a copy with one crate edited took 65.

### Other checkouts and target directories

Results are stored relative to the workspace and the target directory, so a second checkout or a new target directory finds them. For that to be true of the compiled bytes, Jato asks rustc to write `/proc/self/cwd` in place of the workspace's location and `/proc/self/cwd/target` in place of the target directory's, through `--remap-path-prefix`. A debugger started in the workspace opens the right sources through those names. Diagnostics, `file!()` and panic messages are unchanged.

Text inside the target directory that names the workspace or the target directory is known by what it says without those names, to a compiler that reads it as to the build script that wrote it: source generated with full paths in it is the same source in every checkout, and the crate that includes it is compiled once.

Output that still names where it was built, such as a program that embeds `env!("CARGO_MANIFEST_DIR")`, is reused only in that checkout. `JATO_REMAP=0` keeps real paths in everything, and results then stay with the directory they were made in.

A build can also depend on what is around its checkout. A crate named by `path = "../shared"`, or anything else a command reads from near the workspace without the environment naming the place, is recorded twice: where it was, and by the way there from the workspace. A result is reused only where both are as they were, so a checkout with a different neighbour gets a build of its own and one with the same neighbour shares it. Cargo, rustup and git look for settings in every directory above the workspace. What such a search found, apart from Cargo's and rustup's own directories, has to be what a search from the other checkout finds: a directory with a `.cargo/config.toml` above one checkout and not the other keeps their builds apart. A build that writes files beside its workspace is reused only there.

Cargo asks git which files a package has when the package's build script names no inputs, to compare their times with what it built. A repository's own files differ between a checkout, a worktree of it and a clone, and they change with every commit. What Cargo itself reads inside a repository's directory is therefore not counted among a build's inputs: a build made in one worktree is put back in another, on another branch, and after a commit that left the built files alone. What any other program reads there counts like everything else, so a build script that puts the current commit into the program gets a build for each commit. The directories a workspace is inside are not inputs either, and a build outlives the checkout that made it.

On its way to a package's files Cargo lists the top of the repository and every directory of the package. It goes by the names in a directory only where it looks for a package's programs, tests, examples and benchmarks, and where a workspace finds its packages by a pattern such as `crates/*`. What it lists elsewhere is not counted: a file git does not track, an editor's leftovers, or the `target` directory of an earlier build beside a build made into another, keep no build from being put back. A directory that a build script or a compiler lists counts in full.

The files of a build that is put back all get one time, taken from the filesystem and set just before the present, so that no source looks newer than the build and any file edited from then on does. A file outside the target directory that already holds what the build wrote, such as `Cargo.lock`, is left as it is.

### Edits

A crate compiled again within ten minutes of its last compilation in the same target directory is being edited. rustc compiles it incrementally from the state it saved, Jato looks for a recorded result (which is how undoing an edit is instant) and otherwise leaves the compiler unwatched and its output unrecorded, because the next edit replaces it. The first compilation after a pause is recorded again. `JATO_EDIT_WINDOW` sets the pause in seconds; `0` records every compilation.

Watching a compiler is free while processors are free. On a machine where every one is in use, each file the compiler asks about costs it a turn at the processor, and a short compilation can take several times as long: checking `olo-system` and its 54 dependents after an edit took 17 to 25 s watched and 7 to 14 s unwatched at a load of 45 to 55 on 15 processors. So in a target directory that is already built, compilations are left unwatched while the load average is at or above the number of processors, and recorded when it is below. A build made from nothing is always recorded. `JATO_BUSY` sets the load; `0` always watches.

A target directory that was restored holds none of the state rustc saves for compiling a crate again, so the first edit of a crate there would compile the whole crate. Jato keeps that state in the cache, once for each crate and replaced by newer state when it is more than an hour old, and hands it to a compilation that has none of its own: the first edit in a new checkout is compiled incrementally, from state saved where the crate was last compiled under watch. On Olo that took the first edit of the top crate in a new checkout from about 110 processor-seconds to 53.

What rustc makes of the state depends on the crate. For a library it reuses its analysis and the code it generated for every module that was not edited. For a program or a test in another target directory it reuses the analysis and generates all code again: rustc ties the code of anything it links to the real locations of the libraries linked in, whatever names they are given by. In the target directory the state was saved in, after a clean, a program's code is reused too.

rustc accepts saved state only under the options it was made with, and the workspace's location is one of them. Jato sets the few bytes at the start of the state that stand for the options to this checkout's, which it learns by having the compiler compile a one-line crate, and names the output directory through `/proc/self/cwd` so that it reads the same everywhere. rustc still checks each saved result against the sources before using it, and if the state does not begin the way Jato expects it is left out and the crate is compiled from scratch. State is kept for the output directory it was made with, since rustc generates all code again for any other. A target directory inside the workspace reads the same in every checkout. One kept anywhere else, such as a scratch `CARGO_TARGET_DIR` for each checkout, is held open for the compiler on a descriptor and named through that, `/proc/self/fd/219`, when the crate is a library, which rustc writes itself. Programs, tests and procedural macros there are compiled without state the first time: a linker would have to keep the descriptor. `JATO_SAVED_STATE=0` turns this off.

### Builds at the same time

Every compiler started through Jato takes one of as many places as the machine has processors, shared by all builds that use the same cache. Five builds started together then run fifteen compilers on a fifteen-processor machine, not seventy-five. A place is a file lock, so it is given up however its holder ends. `JATO_JOBS` sets the number of places; `0` removes the limit.

Two builds that need the same compilation at the same moment run it once: the second waits for the first and uses its result. `JATO_COALESCE=0` lets them run independently.

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

The watcher uses native filesystem events and Cargo metadata. It follows workspace directories, member directories and direct local path dependencies, honors ignore rules, and leaves out Cargo's outputs and Jato's cache. A new save supersedes a running job, including its compiler and test processes.

`jato dev build`, `run`, `test` and `watch` select the Cargo profile `jato-dev`, which inherits the project's `dev` profile, uses 16 codegen units, keeps line tables for workspace members and omits debug information for dependencies. Its outputs go to `target/jato-dev`. An explicit `--profile` or `--release` takes precedence.

## Settings

| Setting or command | Purpose |
| --- | --- |
| `JATO_CACHE_DIR` | Private directory on a local filesystem. Defaults to `$XDG_CACHE_HOME/jato`, or `~/.cache/jato`. Builds that share it share results and processors. |
| `jato limit 32GiB` | Set the budget for this cache directory. Without one it is a tenth of the filesystem, at least 10 GiB and at most 100 GiB. `jato limit` prints it; `--unset` removes it. |
| `JATO_MAX_SIZE=4GiB` | Budget for this process when none is stored with `jato limit`. |
| `jato gc 10GiB` | Evict least-recently-used results down to this size. `jato gc` uses the budget; `jato gc 0` empties the cache. It also removes what earlier versions of Jato left in the directory. |
| `jato stats [--json]` | Builds put back whole; Rust compilations and build scripts' C compilations reused, run, left to rustc during edits or not storable; time waited for other builds; disk use. |
| `JATO_LOG=debug` | Say for each command whether it was reused and, if not, what had changed. An absolute path appends to that file, with times. |
| `JATO_KEY_ENV=A,B` | Variables a procedural macro reads from the environment. |
| `JATO_JOBS=8` | Compilers running at once across the builds sharing this cache. |
| `JATO_EDIT_WINDOW=0` | Record every compilation, including each edit. |
| `JATO_BUSY=8` | The load average from which compilations in a built target are left unwatched. Defaults to the number of processors; `0` always watches. |
| `JATO_WHOLE_BUILDS=0` | Reuse single compilations only. |
| `JATO_NEAR=0` | Never start a build from a recorded one whose sources differ. |
| `JATO_TOOLS=0` | Leave the C compilers and assemblers that build scripts start to themselves. |
| `JATO_SAVED_STATE=0` | Do not keep rustc's incremental state in the cache or hand it to other checkouts. |
| `JATO_NOTES=0` | Have the kernel report every file call of a watched program; none are told of by the program itself. |
| `JATO_STORAGE=packed` | Keep compressed copies even where files can be cloned. |
| `JATO_VERIFY=1` | Check the digest of restored files on a filesystem that checksums them. |
| `JATO_DISABLE=1` | Run Cargo and the compiler directly. |

## Storage

The cache directory holds an SQLite index and the files commands wrote, each stored once under the BLAKE3 digest of its contents. On a filesystem that can clone files, such as Btrfs or XFS, a stored file is a clone of the compiler's output and a restored file is a clone of that: the bytes exist once on disk however many target directories hold them, and each copy can still be written without affecting the others. On other filesystems objects are compressed with Zstandard and restores are copies.

Measured on Btrfs, for builds and tests of two programs of Olo: a cache of 807 MiB and four target directories filled from it, 2,651 MiB by their sizes, held 22 MiB between them that only one of the five had. Each worktree's target directory had 0 to 3 MiB of its own. One that bare Cargo built beside them had all 153 MiB of its own.

A result becomes visible in one transaction, after its files are in place. Each record says itself which command it belongs to and which files it needs; the index only finds records and counts references. A restored file's digest is checked unless the filesystem checksums data itself. A record that cannot be read or whose files are gone is dropped and the command runs.

Next to results the cache keeps the state rustc saved for each crate of a workspace, which is about as large as the crate's compiled output. It is stored once however many checkouts are handed it.

There is no daemon, and nothing runs between builds. The cache holds compiled artifacts and diagnostics, which may include values embedded by `env!`.

## What is not reused

- A command that failed. Its diagnostics and exit status are the compiler's own every time.
- A command that used the network, talked to a service through a socket Jato does not know to be harmless, or opened files in a way that cannot be watched.
- A command that changed something it had read, other than the outputs compilers and linkers rewrite in place.
- A compilation whose source arrives on standard input.

`jato stats` counts these as not storable, and `JATO_LOG=debug` says why, naming the program and the file or socket where there is one.

What leaves no trace in files or in the variables above is not seen at all: the time of day, a random number, the state of a machine across the network. A C file that asks for the time it is compiled at, through `__TIME__` or `__DATE__`, is given back with the time of its first compilation, as a Rust crate whose procedural macro reads the clock is.

## Design references

- BuildXL and Bazel record what an action read and key its result on that. Jato applies the two-step lookup to Cargo without a build description of its own: Cargo stays in charge, and the observation supplies the dependency list. [BuildXL two-phase lookup](https://github.com/microsoft/BuildXL/blob/main/Documentation/Wiki/Advanced-Features/Two-Phase-Cache-Lookup.md) · [Bazel remote caching](https://bazel.build/remote/caching)
- ccache's direct mode checks a manifest of the files a compilation included before running the preprocessor. Jato's manifests are the same idea with the list taken from the kernel. [ccache manual](https://ccache.dev/manual/latest.html)
- seccomp user notification lets a supervisor see a system call before it happens and let it proceed. A filter can also tell where in a program a call was made from, which is what lets a library in the program vouch for the calls it makes itself. [seccomp_unotify(2)](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html) · [seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html)
- Reflinks let a restored file share its extents with the cache until someone writes to it. [FICLONE](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html)
- kache and sccache cache single rustc invocations keyed on their arguments and declared inputs. Jato is measured against both. [kache](https://github.com/kunobi-ninja/kache) · [sccache](https://github.com/mozilla/sccache) · [Measurements](bench/README.md)

## Development

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. The integration suite builds disposable Cargo projects and checks restored bytes, invalidation, other checkouts and what is beside and above them, procedural macros, build scripts and the C they compile, linkers, damaged caches and concurrent builds. Run `python3 tests/install_skill.py` to check the skill installer. Project conventions, open work and the journal live alongside the code. [AGENTS.md](AGENTS.md) · [TODO.md](TODO.md) · [JOURNAL.md](JOURNAL.md)
