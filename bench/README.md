# Jato measurements

The whole Olo workspace was measured on a build server with Jato 0.17.0 and on a laptop with Jato 0.15.0, what watching costs with 0.12.0 and 0.14.0, everything else with 0.12.0.

Benchmarks ran on an Intel Core Ultra 7 356H (16 logical processors, 15 available to the session, 62 GiB) with Fedora 45, Linux 7.3.4 and Btrfs with zstd compression. The machine was shared with other people's builds throughout: its load average is given with each set of numbers, and wall-clock times moved with it. Comparisons within one table were taken in one sitting, rotating the order of contenders where there was more than one run.

Each contender had its own empty store, its own daemon where it has one, its own copy of the project and its own target directories. The built program was run after every step and had to print the same thing for every contender. Times are wall-clock seconds.

## Olo

`cargo build -p olo-system` at Olo commit `645cce1e3aea`: 279 compilations on the pinned `nightly-2026-11-14` with `-Zthreads=8`, upstream mold 3.0.0, the project's dev profile and workspace feature unification. Each contender built in its checkout's own `target`. One run; load average 21 to 29, except during Jato's cold build, when it was 50.

| Seconds | Jato 0.12.0 | kache 2.3.0 | Bare Cargo |
| --- | ---: | ---: | ---: |
| Nothing stored, nothing built | 145.97 | 113.60 | 128.90 |
| The same build again | 0.09 | 0.60 | 0.55 |
| Target directory removed | 0.20 | 2.63 | a cold build |
| A target directory never used | 0.21 | 3.07 | a cold build |
| The sources copied to another path | 0.33 | 6.77 | a cold build |
| A function added to `olo-system` in that copy | 13.26 | 24.71 | |
| A function added in the restored target | 8.93 | 16.26 | 6.87 |
| Further such edits, median of three | 7.96 | 7.84 | 9.92 |
| The edits undone | 0.12 | 6.34 | 9.01 |

The cold builds used 769, 758 and 763 processor-seconds: watching every compiler and keeping what it wrote cost under 1% more than bare Cargo. Their wall-clock times follow the machine's load, not the contender: over five sittings Jato's cold build took 130 to 146 s and once 662 s, kache's 114 to 339 s, Cargo's 118 to 285 s.

In Jato's restores Cargo does not start; kache answers 279 compiler invocations one after another, and for a target directory it has not seen it fills in what it can before Cargo looks. The first edit in a checkout where nothing has been compiled used 53 processor-seconds with Jato, which handed rustc the state it had saved in the first checkout, and 125 with kache, where rustc compiled the crate from nothing. In the restored target the same took 21 against 47 processor-seconds; Cargo's own target, built in place, still had rustc's state and took 19. Before Jato handed saved state on, both of these edits cost it 108 to 127 processor-seconds.

After the cold build Jato's store held 1,212 MiB: the build's outputs (616 MiB) and the state rustc saved for the workspace's 15 crates. 12 MiB of that was not shared with the target directory it was cloned from. kache's store held 625 MiB with 11 MiB of its own; kache builds without incremental state. The target directory was 1,209 MiB under Jato and bare Cargo and 646 MiB under kache. A target Jato restores takes 1.3 MiB of its own until something in it is compiled. kache's daemon held 21 MiB of memory between builds; Jato has no daemon. [Samples, processor time and disk accounting](olo-0.12.json)

## The whole Olo workspace

`cargo check --workspace` at Olo commit `392b862f3`, in a clone of its git repository: 121 packages, 1,502 Rust compilations, and 113 build scripts that between them compile about 1,100 C, C++ and assembly files. Each contender handled C its own way: kache as `kache cc` in `CC`, `CXX`, `HOST_CC` and `HOST_CXX`, as its setup leaves them, and Jato through the names it puts on `PATH`. Jato 0.15.0 and kache 2.3.0 went through the steps at the same time, each in its own copy with its own store, so that both met the same machine. One run; the load average was 29 to 38 at the start of each step, and 52 at kache's last, which ran after Jato had finished.

| Seconds, processor-seconds | Jato 0.15.0 | | kache 2.3.0 | |
| --- | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 913.60 | 2,319 | 938.01 | 2,297 |
| The same check again | 0.83 | 0.6 | 1.37 | 0.9 |
| Target directory removed | 2.21 | 1.7 | 18.00 | 55 |
| A target directory never used | 2.49 | 1.9 | 30.33 | 117 |
| The sources copied to another path | 3.78 | 2.4 | 13.22 | 42 |
| A function added to `olo-system` in that copy | 8.39 | 31 | 16.63 | 91 |
| A third copy, the function added before anything is built there | 14.76 | 39 | 32.84 | 126 |
| A function added in the restored target | 10.84 | 29 | 14.31 | 32 |
| Further such edits, median of three | 8.47 | 23 | 24.53 | 27 |
| The edits undone | 0.92 | 0.7 | 41.76 | 28 |

Putting a build back is one command's result for Jato and 2,600 for kache, of which the C compilations and the build scripts that start them are most of the processor time. `olo-system` has 54 dependents in the workspace, so each edit of it compiles 55 crates.

The third copy is what a new worktree with an edit in it is: sources no build was made from. Jato starts from the nearest recorded build, puts its files in place without what the compiler of `olo-system` wrote, and has Cargo compile that crate and its dependents; kache answers every compiler and runs every build script. Measured separately in git worktrees of the clone, at a load of 18 to 25: a first build with nothing changed 2.3 s, with one line changed in one program 7.2 s, with a comment added to a crate at the bottom of the workspace 5.9 s, with a function added to `olo-system` and committed 11.6 s and 40 processor-seconds, and the next worktree with any of those sources 1.8 to 2.2 s. Without starting from an earlier build (`JATO_NEAR=0`), as Jato did until 0.15, the one line took 22.6 s and 75 processor-seconds and the function 37.2 s and 107.

Clippy over the same workspace, in a worktree where the check had been put back, compiled the 121 packages with its own driver for 321 processor-seconds and was recorded as resting on the check, whose 13,088 files it holds with its own. `clippy --workspace` as the first command in the next new worktree was put back in 5.3 s, at a load of 61.

From nothing the two take the same time and the same processor time when they run side by side. One after the other on this machine they do not, because other people's builds come and go by the minute: over four such sittings Jato's check from nothing took 508, 1,490, 670 and 1,890 s and kache's 547, 642, 428 and 548 s, each time for 2,250 to 2,730 processor-seconds, and whichever ran while the machine was busiest took longest. The desktop this machine runs keeps compilers in a group that gets a hundredth of the processors' time whenever anything else wants them, so a build's time on the clock says more about what else ran than about the build. Comparisons of builds from nothing here are only meaningful side by side.

Before Jato remembered the C that build scripts compile (0.13), a copied checkout was its worst case: every Rust compilation was restored, the whole build was not, because two C object files with debugging information named the first checkout, and Cargo ran all 113 build scripts, which compiled everything again. That took 929 s and 630 processor-seconds at a load of 40 to 70, 545 of them in C compilers and assemblers.

After the check from nothing Jato's store held 3,100 MiB and kache's 1,608 MiB, the target directories 3,071 and 1,600 MiB: rustc's incremental state, which kache builds without and which is what makes the edits above cheap. Of Jato's target directory 16 MiB was its own; the rest is shared with the store, block for block, and a restored target directory shares all of it. kache's daemon held 42 MiB between builds. [Jato's samples](olo-workspace-0.15-jato.json), [kache's](olo-workspace-0.15-kache.json), and the runs with 0.13.0 ([first](olo-workspace-0.13-watched.json), [second](olo-workspace-0.13.json))

## The whole Olo workspace on a build server

Jato 0.17.0 and kache 2.3.0 on a server with 32 processors (AMD EPYC 9655), 125 GiB of memory, Ubuntu 26.04, Linux 7.0 and ext4 on a disk that writes 7.8 GB a second, with nothing else running. Olo at commit `d01bd55a`, the pinned `nightly-2026-11-14`, mold 2.40.4. The two were started together, one command each, so each had the other's load beside it: Jato was done first both times, and kache's last steps ran on a quieter machine than Jato's. One run each. The load is the one-minute average when the step began.

`check --workspace`:

| Seconds, processor-seconds, load | Jato | | | kache | | |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 169.51 | 2,444 | 5 | 172.80 | 2,479 | 5 |
| The same check again | 0.25 | 0.3 | 50 | 0.44 | 0.4 | 46 |
| Target directory removed | 0.71 | 3 | 46 | 13.72 | 97 | 43 |
| A target directory never used | 0.88 | 3 | 46 | 12.98 | 94 | 38 |
| The sources copied to another path | 0.99 | 3 | 46 | 12.31 | 108 | 34 |
| A function added to `olo-system` in that copy | 4.02 | 29 | 46 | 10.43 | 199 | 29 |
| A third copy, the function added before anything is built there | 7.01 | 36 | 43 | 17.01 | 174 | 30 |
| A fourth copy, a note added to `olo-system`'s manifest first | 3.90 | 8 | 39 | 12.03 | 143 | 22 |
| A function added in the restored target | 4.35 | 27 | 38 | 8.17 | 153 | 22 |
| Further such edits, median of three | 3.88 | 23 | 34 | 3.14 | 27 | 23 |
| The edits undone | 0.46 | 0.5 | 31 | 3.06 | 26 | 20 |

`build --workspace`, the dev profile, 56 programs:

| Seconds, processor-seconds, load | Jato | | | kache | | |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 513.21 | 7,878 | 14 | 508.38 | 7,773 | 14 |
| The same build again | 0.23 | 0.2 | 62 | 0.76 | 0.5 | 67 |
| Target directory removed | 5.31 | 24 | 58 | 28.11 | 139 | 62 |
| A target directory never used | 7.17 | 22 | 56 | 134.60 | 2,128 | 48 |
| The sources copied to another path | 13.74 | 47 | 54 | 20.47 | 113 | 62 |
| A function added to `olo-system` in that copy | 61.76 | 703 | 48 | 111.19 | 1,449 | 56 |
| A third copy, the function added before anything is built there | 74.50 | 817 | 61 | 95.06 | 1,093 | 64 |
| A fourth copy, a note added to `olo-system`'s manifest first | 55.01 | 710 | 58 | 15.79 | 215 | 23 |
| A function added in the restored target | 34.98 | 164 | 57 | 38.18 | 650 | 22 |
| Further such edits, median of two | 15.06 | 65 | 60 | 13.47 | 64 | 25 |
| The edits undone | 11.50 | 12 | 82 | 9.81 | 66 | 20 |

From nothing the two are the same: 2% apart either way, with 1% between their processor times. Everything that puts a build back is Jato's by a wide margin, a second where kache takes thirteen for the check, and five to seven where kache takes 28 and 135 for the build.

kache is ahead in three places. Further edits in a target directory that is built take both about as long, and kache's were measured at half the load: 3.1 s against 3.9 for the check, 13.5 against 15.1 for the build. Alone on the quiet machine the same edits of the check took Jato 2.9 to 3.2 s, of which Cargo and rustc took 2.8 to 3.0. Undoing the edits of the build took Jato 11.5 s at a load of 82, 12 processor-seconds of it its own, to put the programs back that the edits had linked again; kache took 9.8 s and 66 processor-seconds.

The third is the note in a manifest, for the build: 55 s against 16. A procedural macro `olo-system` uses reads the crate's own `Cargo.toml`. Jato sees that and has the crate compiled again; Cargo and kache know nothing of it and do not. For the check that costs one compilation, since what the crates above read of `olo-system` comes out the same byte for byte and they are answered from the cache. For the build the library comes out as other bytes, as it does every time rustc compiles it (the object files in it are named with a string made up for each compilation), and the 40 programs that link it are linked again. Run alone on a quiet machine the same step took 34 s. With a manifest nothing compiles from, the build starts from the recorded one and Cargo finds nothing to do: 2.1 s for the check.

After the build from nothing Jato's store held 5,703 MiB and kache's 12,319 MiB: on a filesystem that cannot clone, Jato keeps what it stores compressed. The target directories were 20,543 and 12,360 MiB, the difference being rustc's incremental state. For the check: 1,281 and 1,716 MiB stored, 3,073 and 1,602 MiB of target directory.

The same check of real commits, each in a new worktree against a record of the newest commit, Jato alone on the quiet machine: one commit back, where the lock file and a manifest differ and one build script has to run again, 3.1 s (10.2 s when Cargo carried the build out, before builds started across manifests); five commits back 9.4 s with 88 compilers run; forty commits back, 181 inputs differing, 20 s with 255 compilers run and 2,035 files of the earlier build taken away. Each was then compared with a check of the same commit from nothing with a cache of its own: the same files, but for 41 object files that the `cc` crate names after the path of its output directory. And every compilation was asked for one at a time into another target directory: all answered by records that match, but for the eight crates downstream of a build script that writes its source differently every time.

Reproduce with one command for each contender, started together:

```sh
python3 bench/compare.py --offline --project /path/to/olo --command check --c-compilers own \
    --edit crates/system/src/lib.rs --tools jato --jato /path/to/jato --kache /path/to/kache \
    --runs 1 --cache-gib 60 --scratch /path/on/disk --cargo-args --workspace
```

The data is in `bench/olo-server-0.17-{check,build}-{jato,kache}.json`.

## ripgrep

ripgrep 14.1.1 on Rust 1.100, default dev profile, median of three rotating runs; load average 21 to 30. `jato` is `jato build`; `jato wrapper` is plain Cargo with Jato as `RUSTC_WRAPPER`, which reuses single compilations only.

| Seconds | `jato` | `jato` wrapper | kache 2.3.0 | sccache 0.19.0 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 10.93 | 16.77 | 7.33 | 8.53 | 7.83 |
| The same build again | 0.01 | 0.04 | 0.03 | 0.03 | 0.03 |
| Target directory removed | 0.07 | 0.18 | 0.38 | 4.86 | 9.53 |
| A target directory never used | 0.34 | 0.17 | 0.61 | 8.09 | 9.12 |
| The sources copied to another path | 0.08 | 0.19 | 0.70 | 7.54 | 11.32 |
| A function added in that copy | 2.13 | 3.23 | 2.94 | 0.50 | 0.68 |
| A function added in the restored target | 1.06 | 0.91 | 2.07 | 0.47 | 0.77 |
| Further such edits, median of three | 0.52 | 0.51 | 0.53 | 0.47 | 0.50 |
| The edits undone | 0.05 | 0.05 | 0.16 | 0.51 | 0.69 |

The cold builds used 25.8 to 27.7 processor-seconds with `jato`, 25.6 to 28.3 as a wrapper, 23.2 to 23.6 with kache, 26.7 to 28.4 with sccache and 24.4 to 25.6 with bare Cargo: on a project this small, watching the compilers and keeping rustc's state for nine crates costs 5 to 8%. The cold wall-clock samples of one contender spread from 8 to 22 s as the machine's load moved; a build of the same kind alone at a load of 15 took 5.7 s. In those runs each wrapper kept rustc's state before telling Cargo it was done; under `jato build` that now happens after.

For sccache and bare Cargo the copy is a cold build, which leaves rustc's own state behind for the edit that follows; sccache does not cache the workspace's incrementally compiled crates at all. For a crate this small the state Jato hands on is worth little over compiling from nothing: 2.1 s against kache's 2.9 s in the copy. [Samples and disk accounting](ripgrep-0.12.json)

## Builds started together

Eight builds of ripgrep started at the same moment, each in its own copy with a compiler setting of its own so that none could use another's results. `jato` has its default of 15 compilers at once; `jato, no limit` is `JATO_JOBS=0`. Two runs, taken while the machine's load average before each start was between 27 and 97, which is why the times of one contender differ by as much as they do; the order was rotated.

| | `jato` | `jato`, no limit | kache 2.3.0 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: |
| Most compilers running at once | 15 | 119, 120 | 16 | 117, 112 |
| Most memory resident at once | 4.1, 3.9 GiB | 12.7, 12.9 GiB | 3.8, 3.8 GiB | 14.3, 13.9 GiB |
| Processor-seconds | 225, 242 | 244, 281 | 225, 290 | 223, 235 |
| Seconds until the last build finished | 75, 103 | 95, 226 | 78, 390 | 47, 116 |
| Load average just before | 77, 70 | 68, 63 | 77, 74 | 27, 97 |

Without the limit eight builds ran 119 compilers on 15 processors and held three times the memory. With it they used 8 to 14% less processor time and finished sooner in both runs. kache's daemon limits compilers in the same way. Bare Cargo's first run happened to start on a much quieter machine than the others'. Memory is the sum of resident sizes, which counts pages shared between compilers once for each. [Samples](concurrent-ripgrep-0.12.json)

## Checks on save

`jato watch` on a workspace of two crates, seven saves each way, while the machine's load average was above 50. A save that breaks the code reached the compiler's error in a median 137 ms, and the save that repairs it reached a clean check of the workspace in 154 ms. Four further runs alternated the cache on and off (`JATO_DISABLE=1`) at a load of 56 to 58: 129 and 150 ms to the error with it on against 160 and 179 ms with it off, and 153 and 173 ms to the clean check against 259 and 338 ms, since the repair returns to a state that is already recorded. The idle watcher held 4.5 MiB. [Samples of the first run](feedback-0.12.json)

## What observation costs

Measured with 0.12, where the kernel reported every file call: the same incremental compilation of `olo-system` (1.1 to 1.7 s unwatched) was run alternately with and without the observer. With the machine's load average at 22 the two were indistinguishable: 1.07 to 1.44 s unwatched, 1.00 to 1.69 s watched and recorded. With it between 35 and 50 the watched runs took 2.1 to 2.4 s: each of the reported system calls waits for the observer to be given a processor, and the compiler for its next turn after that. Checking `olo-system` and its 54 dependents after an edit took 17 to 25 s watched and 7 to 14 s unwatched at a load of 45 to 55.

With 0.14 programs tell of their own reads. For one check of `olo-system`, three runs each way in alternation at a load of 23:

| | Unwatched | Watched, kernel alone | Watched, with notes |
| --- | ---: | ---: | ---: |
| Calls the kernel reported | | 2,600 before, 796 without listings | 158 |
| Times the command gave up the processor | 3,500 to 5,400 | 20,900 to 22,200 | 6,950 to 7,980 |
| Seconds | 1.86 to 2.02 | 1.99 to 3.12 | 1.81 to 2.81 |

A build from nothing has slack that hides those waits: two cold checks of `olo-system` run side by side at a load of 50, one watched each way, took 397.88 and 397.91 s for 429 and 430 processor-seconds. ripgrep from nothing at a load of 31, three runs in alternation: bare Cargo 8.2 to 9.5 s and 25.0 to 26.5 processor-seconds, Jato with the kernel alone 9.9 to 11.5 s and 27.0 to 28.2, Jato with notes 9.7 to 10.8 s and 26.9 to 28.5.

## Reproduction

Use prefetched dependencies and a scratch directory on disk. `--no-rebuilds cargo` leaves out the steps in which bare Cargo would only repeat its cold build.

```sh
RUSTUP_TOOLCHAIN=stable python3 bench/compare.py --offline --project /path/to/ripgrep-14.1.1 \
    --edit crates/core/main.rs --verify "rg --version" \
    --tools cargo jato jato-wrapper kache sccache --runs 3
python3 bench/compare.py --offline --project /path/to/olo --package olo-system \
    --edit crates/system/src/lib.rs --verify "nm-fake --help" \
    --tools cargo jato kache --no-rebuilds cargo --runs 1
# Side by side: one command for each contender, started together, each with a scratch directory of its own.
for tool in jato kache; do
    python3 bench/compare.py --offline --project /path/to/olo --command check --c-compilers own \
        --edit crates/system/src/lib.rs --tools $tool --runs 1 --cache-gib 40 \
        --scratch /var/tmp/compare-$tool --cargo-args --workspace > olo-workspace-$tool.json &
done; wait
RUSTUP_TOOLCHAIN=stable python3 bench/concurrent.py --offline --project /path/to/ripgrep-14.1.1 \
    --verify "rg --version" --builds 8 --tools cargo jato jato-unlimited kache
python3 bench/feedback.py --jato /path/to/jato
```

`compare.py` takes each contender through a cold build, the same build again, the target directory removed, a new target directory, a copy of the sources at another path, a second copy that is edited before anything is built in it, edits to one file and their undoing. Contenders named in one command run one after the other. On a machine other people are building on, start one command for each contender at the same moment instead, as above: their builds from nothing then meet the same load. By default build scripts are given the C compilers by their paths and no contender caches C; `--c-compilers own` gives kache and sccache their wrappers and leaves Jato the names it puts on `PATH`. `jato` is Jato run as `jato build`; `jato-wrapper` is Jato named as `RUSTC_WRAPPER` under plain Cargo, which reuses single compilations only. `concurrent.py` starts several builds at once that share nothing, to show how they share the machine. `feedback.py` measures a save to the compiler's diagnostic under `jato watch`.
