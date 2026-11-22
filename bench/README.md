# Jato 0.12 measurements

Benchmarks ran on an Intel Core Ultra 7 356H (16 logical processors, 15 available to the session, 62 GiB) with Fedora 45, Linux 7.3.4 and Btrfs with zstd compression. The machine was shared with other people's builds throughout: its load average is given with each set of numbers, and wall-clock times moved with it. Comparisons within one table were taken in one sitting, rotating the order of contenders where there was more than one run.

Each contender had its own empty store, its own daemon where it has one, its own copy of the project and its own target directories. The built program was run after every step and had to print the same thing for every contender. Times are wall-clock seconds.

## Olo

`cargo build -p olo-system` at Olo commit `645cce1e3aea`: 279 compilations on the pinned `nightly-2026-11-14` with `-Zthreads=8`, upstream mold 3.0.0, the project's dev profile and workspace feature unification. One run; load average 26 to 36.

| Seconds | Jato 0.12.0 | kache 2.3.0 | Bare Cargo |
| --- | ---: | ---: | ---: |
| Nothing stored, nothing built | 129.70 | 139.06 | 141.76 |
| The same build again | 0.07 | 0.45 | 0.48 |
| Target directory removed | 0.17 | 2.39 | a cold build |
| A target directory never used | 0.30 | 1.54 | a cold build |
| The sources copied to another path | 0.22 | 4.30 | a cold build |
| First edit of `olo-system` in the restored target | 21.64 | 30.73 | 2.25 |
| Further edits, median of three | 1.87 | 4.37 | 2.17 |
| The edits undone | 0.07 | 2.61 | 2.44 |

The cold builds used 746, 766 and 762 processor-seconds: watching every compiler cost nothing that shows next to the difference between runs. In Jato's restores Cargo does not start; kache answers 279 compiler invocations one after another. Both caches compile the edited crate from scratch the first time, because a restored target has none of rustc's saved state; Cargo, whose target was built in place, has it. For a target directory it has not seen, kache fills in what it can before Cargo looks, and Cargo then asked it for 11 compilations.

After the cold build Jato's store held 616 MiB of files, of which 5 MiB were not shared with the target directory they were cloned from; kache's held 625 MiB with 11 MiB of its own. The target directory was 1,209 MiB under Jato and bare Cargo, 602 MiB of that rustc's incremental state and other files nothing else shares, and 646 MiB under kache, which built without incremental compilation. A target Jato restores has no incremental state until something in it is compiled. kache's daemon held 21 MiB of memory between builds; Jato has no daemon. [Samples, processor time and disk accounting](olo-0.12.json)

## ripgrep

ripgrep 14.1.1 on Rust 1.100, default dev profile, median of three rotating runs; load average 20 to 32. `jato` is `jato build`; `jato wrapper` is plain Cargo with Jato as `RUSTC_WRAPPER`, which reuses single compilations only.

| Seconds | `jato` | `jato` wrapper | kache 2.3.0 | sccache 0.19.0 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 8.38 | 11.74 | 9.19 | 13.63 | 8.99 |
| The same build again | 0.01 | 0.03 | 0.03 | 0.03 | 0.03 |
| Target directory removed | 0.06 | 0.17 | 0.41 | 5.22 | 7.08 |
| A target directory never used | 0.05 | 0.14 | 0.29 | 15.65 | 8.87 |
| The sources copied to another path | 0.07 | 0.16 | 0.40 | 8.23 | 8.41 |
| First edit in the restored target | 1.75 | 3.19 | 3.08 | 0.63 | 0.54 |
| Further edits, median of three | 0.46 | 0.63 | 0.69 | 0.44 | 0.51 |
| The edits undone | 0.04 | 0.05 | 0.18 | 0.40 | 0.43 |

Cold builds used 25.4, 25.0, 25.2, 27.7 and 25.1 processor-seconds in that order. The cold wall-clock times of a single contender ranged over 7 to 15 s between runs, more than the contenders differ from each other. sccache does not cache the incrementally compiled workspace crates, so its target keeps rustc's state and its first edit is Cargo's. [Samples and disk accounting](ripgrep-0.12.json)

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

## What observation costs

The same incremental compilation of `olo-system` (1.1 to 1.7 s unwatched) was run alternately with and without the observer. With the machine's load average at 22 the two were indistinguishable: 1.07 to 1.44 s unwatched, 1.00 to 1.69 s watched and recorded. With it between 35 and 50 the watched runs took 2.1 to 2.4 s: each of about 5,700 reported system calls waits for the observer to be given a processor. This is why a crate that is being edited is compiled unwatched, and why cold builds, which take minutes, show no cost.

## Reproduction

Use prefetched dependencies and a scratch directory on disk. `--no-rebuilds cargo` leaves out the steps in which bare Cargo would only repeat its cold build.

```sh
RUSTUP_TOOLCHAIN=stable python3 bench/compare.py --offline --project /path/to/ripgrep-14.1.1 \
    --edit crates/core/main.rs --verify "rg --version" \
    --tools cargo jato jato-wrapper kache sccache --runs 3
python3 bench/compare.py --offline --project /path/to/olo --package olo-system \
    --edit crates/system/src/lib.rs --verify "nm-fake --help" \
    --tools cargo jato kache --no-rebuilds cargo --runs 1
RUSTUP_TOOLCHAIN=stable python3 bench/concurrent.py --offline --project /path/to/ripgrep-14.1.1 \
    --verify "rg --version" --builds 8 --tools cargo jato jato-unlimited kache
python3 bench/feedback.py --jato /path/to/jato
```

`compare.py` takes each contender through a cold build, the same build again, the target directory removed, a new target directory, a copy of the sources at another path, edits to one file and their undoing. `jato` is Jato run as `jato build`; `jato-wrapper` is Jato named as `RUSTC_WRAPPER` under plain Cargo, which reuses single compilations only. `concurrent.py` starts several builds at once that share nothing, to show how they share the machine. `feedback.py` measures a save to the compiler's diagnostic under `jato watch`.
