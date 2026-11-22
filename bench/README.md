# Jato measurements

The whole Olo workspace was measured with Jato 0.13.0 and again with 0.14.0, what watching costs with 0.12.0 and 0.14.0, everything else with 0.12.0.

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

`cargo check --workspace` at Olo commit `97c9f58e4`: 121 packages, 1,502 Rust compilations, and 113 build scripts that between them compile about 1,100 C, C++ and assembly files. Each contender handled C its own way: kache as `kache cc` in `CC`, `CXX`, `HOST_CC` and `HOST_CXX`, as its setup leaves them, and Jato through the names it puts on `PATH`. One run with Jato 0.13.0, kache first; the load average was 29 to 41 at the start of each step and rose past 65 during Jato's cold check.

| Seconds, processor-seconds | Jato 0.13.0 | | kache 2.3.0 | |
| --- | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 1,489.86 | 2,613 | 642.31 | 2,439 |
| The same check again | 0.67 | 0.5 | 1.14 | 1.0 |
| Target directory removed | 1.30 | 2.1 | 13.59 | 53 |
| A target directory never used | 1.04 | 2.1 | 25.58 | 119 |
| The sources copied to another path | 2.22 | 2.2 | 13.11 | 66 |
| A function added to `olo-system` in that copy | 9.74 | 31 | 16.47 | 90 |
| A function added in the restored target | 7.07 | 27 | 13.14 | 32 |
| Further such edits, median of three | 5.10 | 22 | 35.72 | 60 |
| The edits undone | 0.36 | 0.3 | 10.32 | 31 |

Putting a build back is one command's result for Jato and 2,600 for kache, of which the C compilations and the build scripts that start them are most of the processor time. Before Jato remembered C, the copied checkout was the worst case it had: every Rust compilation was restored, the whole build was not, because two C object files with debugging information named the first checkout, and Cargo ran all 113 build scripts, which compiled everything again. That took 929 s and 630 processor-seconds at a load of 40 to 70, 545 of them in C compilers and assemblers.

The check from nothing is where Jato pays for watching. The processor time is the same as kache's within what two runs differ by: 2,449 against 2,609 in a run the day's first, with Jato first, and 2,613 against 2,439 in this one. The time on the clock follows the machine: 508 s against kache's 547 s in the first run, where the load average was 18 when Jato started, and 1,490 s against 642 s here. With every processor in use, each file a watched compiler asks about costs it a turn at the processor. The checklist describes watching from inside the compiler's own process, which would end that.

In the first run compilations in a built target were still watched whatever the load, and the edits went the other way: 18.14 s for the first edit after the restore against kache's 7.01 s, and 8.98 s against 5.18 s for the later ones, for the same processor time. `olo-system` has 54 dependents in the workspace, and a check of each is one short compilation. They are now left unwatched while the load average is at or above the number of processors, which gave the numbers above. kache's times for the same steps moved between the runs by more than that. [Samples and disk accounting](olo-workspace-0.13.json) · [the first run](olo-workspace-0.13-watched.json)

With 0.14.0, in which programs tell of their own reads, the same cold check took 703 s at a load of 30 to 46 and 2,350 processor-seconds; 2.4 million file calls were told of by the programs and 0.54 million reported by the kernel. A copied checkout was put back in 1.7 to 3.8 s, and `check --workspace` in a copy with one low-level crate edited took 24.8 s and 76 processor-seconds: 2,616 results reused, the edited crate and its dependents compiled.

After the cold check Jato's store held 3,099 MiB and kache's 1,609 MiB, the target directories 3,069 and 1,600 MiB: rustc's incremental state again, which kache builds without. kache's daemon held 62 to 82 MiB between builds.

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
python3 bench/compare.py --offline --project /path/to/olo --command check --c-compilers own \
    --edit crates/system/src/lib.rs --tools jato kache --runs 1 --cache-gib 40 \
    --cargo-args --workspace
RUSTUP_TOOLCHAIN=stable python3 bench/concurrent.py --offline --project /path/to/ripgrep-14.1.1 \
    --verify "rg --version" --builds 8 --tools cargo jato jato-unlimited kache
python3 bench/feedback.py --jato /path/to/jato
```

`compare.py` takes each contender through a cold build, the same build again, the target directory removed, a new target directory, a copy of the sources at another path, edits to one file and their undoing. By default build scripts are given the C compilers by their paths and no contender caches C; `--c-compilers own` gives kache and sccache their wrappers and leaves Jato the names it puts on `PATH`. `jato` is Jato run as `jato build`; `jato-wrapper` is Jato named as `RUSTC_WRAPPER` under plain Cargo, which reuses single compilations only. `concurrent.py` starts several builds at once that share nothing, to show how they share the machine. `feedback.py` measures a save to the compiler's diagnostic under `jato watch`.
