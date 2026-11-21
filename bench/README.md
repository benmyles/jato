# Jato 0.4 measurements

The workloads cover a new target directory, a copied checkout, packed writes after an edit, and save-to-diagnostic latency. The workstation ran Fedora 45 with Rust 1.100.0, four Cargo jobs, and Zstandard-compressed Btrfs on an Intel Core Ultra 7 356H. Other users and processes shared the machine. The individual samples include wall time, compiler cache hits, and peak process memory. [0.3 measurements](0.3.md) · [0.2 measurements](0.2.md) · [0.1 measurements](0.1.md)

## Independent checkouts

Each contender starts with an empty private local cache. The harness builds once, immediately repeats the build with Cargo’s fresh target, then rotates contender order for three trials. In each trial it deletes the original target, selects a previously unused target directory, and builds from a separate source copy with its own target. All runs use the same locked dependencies, toolchain, profile, system linker, and job count. The fixtures execute after each release build.

Jato runs through `worktree --root .`. Upstream kache 2.2.0 runs as Cargo’s Rust wrapper with a private local daemon. Upstream sccache 0.19.0 also gets a private daemon and its documented `SCCACHE_BASEDIRS` option, with every source and target root listed before startup. The comparison covers these local configurations. Each tool counts hits at its own level. GNU time reports the largest process in the timed command tree. A separately running peer daemon and its children can fall outside that measurement. [kache worktree support](https://github.com/kunobi-ninja/kache/blob/v2.2.0/README.md#see-it-reuse-a-build) · [sccache path normalization](https://github.com/mozilla/sccache/blob/v0.19.0/README.md#normalizing-paths-with-sccache_basedirs)

| Release workload | Jato 0.4 worktree | kache 2.2 | sccache 0.19 with base directories |
| --- | ---: | ---: | ---: |
| ripgrep, first build | 27.11 s | 22.06 s | 14.84 s |
| ripgrep, unchanged target | 50 ms | 100 ms | 67 ms |
| ripgrep, clean original target | 253 ms | 330 ms | 7.10 s |
| ripgrep, new target | 184 ms | 329 ms | 15.30 s |
| ripgrep, new checkout and target | 273 ms | 380 ms | 15.50 s |
| Regex fixture, first build | 7.21 s | 6.23 s | 4.27 s |
| Regex fixture, unchanged target | 32 ms | 72 ms | 44 ms |
| Regex fixture, clean original target | 108 ms | 132 ms | 640 ms |
| Regex fixture, new target | 89 ms | 133 ms | 5.58 s |
| Regex fixture, new checkout and target | 109 ms | 180 ms | 4.98 s |

The first-build and unchanged-target rows are single observations. The three restore rows show medians. Across all three copied-checkout trials, Jato reported 39 compiler hits for ripgrep and six for the regex application. Its median peak process RSS in those trials was 93.2 MiB and 56.5 MiB respectively. The cold builds took longest with Jato on both workloads. [ripgrep trials, memory and counters](worktree-ripgrep-0.4.json) · [Regex trials, memory and counters](worktree-regex-0.4.json)

Jato’s path mapping is opt-in. Applications can embed the view paths in debug data and path-valued macros. The checkout and its Cargo state remain independently writable. The ripgrep 14.1.1 integration harness passed all 299 tests against an executable with a confirmed Jato hit in a new checkout.

## Ordinary Cargo paths

A separate three-trial regex comparison used an empty target in another checkout. Bare Cargo took 9.01 s, ordinary Jato took 9.67 s, and Jato worktree took 100 ms. Ordinary Jato restored the original target in 75 ms, and Cargo’s unchanged Jato build took 26 ms. These runs used the 0.4 cache engine before the final terminal-signal change. [Physical and projected path samples](path-modes-regex-0.4.json)

Seven alternating clean-target ripgrep comparisons with prefilled caches put Jato 0.4 at 208 ms for check and 246 ms for release. Jato 0.3 took 212 ms and 242 ms respectively. These controls and the zlib/embedded smoke builds used the 0.4 engine at commit `4989335`. The subsequent terminal signal change applies to worktree commands. [All paired samples](paired-0.4.json) · [Native zlib fixture](zlib-0.4.json) · [12 MiB embedded asset](embedded-0.4.json)

## Packed writes across revisions

| Eight release builds | Logical outputs | Packed payload, either version | Median edit, 0.3 | Median edit, 0.4 | Raw bytes reused from earlier entries by 0.4 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Change an exported symbol’s length around a 4 MiB table | 280.0 MiB | 90.9 MiB | 464 ms | 434 ms | 18.75 MiB |
| Change one byte of a 16 MiB static asset | 384.1 MiB | 37.3 MiB | 661 ms | 609 ms | 218.75 MiB |

Each implementation builds the initial revision and seven edits, clears the target, then restores the initial archive. Both produced the same SHA-256 for each workload. For the static asset, filesystem output accounting for the Cargo/compiler/wrapper processes fell **22.1%**, from 2,090,520 to 1,628,232 Linux output blocks. Wall time for all eight builds, including the cold build, was 5.48 s with 0.3 and 5.82 s with 0.4. The symbol-change sequence took 3.73 s and 3.48 s respectively. [Asset revisions and I/O](asset-history-0.4.json) · [Symbol revisions and I/O](history-0.4.json)

A separate storage test stages two 272 MiB artifacts with 1,088 distinct frames, restores the original, verifies its whole-file BLAKE3, and evicts both. The process peaked at 7,776 KiB RSS, measured directly with GNU time on the debug test executable. The fixture uses sparse, compressible input to exercise the index spill with modest physical disk use. The cache uses the file and frame reference indexes to reclaim packed payloads after their final reference disappears.

## Save to compiler feedback

| Two-package workspace, 11 saves | First compiler error | Fix to full workspace success | Idle process-tree PSS |
| --- | ---: | ---: | ---: |
| Ordinary Jato watch | 53.5 ms | 54.9 ms | 2,101 KiB |
| Worktree watch | 53.6 ms | 56.5 ms | 2,894 KiB |

The fixture breaks a local dependency, waits for rustc’s error and the workspace failure, then repairs it and waits for workspace success. The workspace remains open through the sequence. Linux proportional set size apportions shared pages across processes and includes the worktree supervisor. [Ordinary watch samples](feedback-0.4.json) · [Worktree watch samples](worktree-feedback-0.4.json)

## Reproduce

Install Jato and the peer binaries and prefetch the locked projects into Cargo’s registry. Use a local, disk-backed scratch directory. A ripgrep 14.1.1 source archive from crates.io contains the locked upstream workspace used here. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1)

```sh
python3 bench/worktrees.py --tools worktree kache sccache_basedirs --runs 3 --jobs 4
python3 bench/worktrees.py --tools worktree kache sccache_basedirs --runs 3 --jobs 4 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg
python3 bench/history.py --jato jato --previous-jato /path/to/jato-0.3.0
python3 bench/history.py --jato jato --previous-jato /path/to/jato-0.3.0 --edit-asset
python3 bench/feedback.py --runs 11
python3 bench/feedback.py --worktree --runs 11
cargo test --locked --bin jato large_packed_revision_spills -- --ignored
```

All harnesses accept `--scratch`. The checkout comparison also accepts `--jato`, `--kache` and `--sccache` paths, `--mode check`, a bare Cargo contender, and ordinary Jato and sccache wrappers. Use `bench/compare.py --help` for build, edit, peak RSS and extent comparisons at a fixed physical target.
