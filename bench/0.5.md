# Jato 0.5 measurements

Two different worktrees can ask Cargo to compile the same expensive crate at the same time. The cold tests start the builds together with independent empty targets and a shared empty cache. The warm tests switch between familiar checkouts. A separate watcher workload measures registration and adding directories during an edit session. [0.4 report](0.4.md) · [0.3 report](0.3.md) · [0.2 report](0.2.md) · [0.1 report](0.1.md)

## Concurrent cold builds

All contenders use Rust 1.100.0 on a shared Intel Core Ultra 7 356H workstation running Fedora 45 and compressed Btrfs. Each trial gets a new local cache. Jato uses `worktree --root .`, kache 2.2.0 uses its private local Rust wrapper and daemon, and sccache 0.19.0 gets every source and target root in `SCCACHE_BASEDIRS` before its private server starts. Each compiled executable runs and its output is compared. [kache’s in-flight compilation support](https://github.com/kunobi-ninja/kache/blob/v2.2.0/README.md#why-use-kache) · [sccache path configuration](https://github.com/mozilla/sccache/blob/v0.19.0/README.md#normalizing-paths-with-sccache_basedirs)

| Two cold ripgrep release builds, four jobs per checkout | Jato 0.5 | Jato 0.4 | kache 2.2 | sccache 0.19 |
| --- | ---: | ---: | ---: | ---: |
| Wall time to both executables | **25.92 s** | 59.96 s | 31.63 s | 32.02 s |
| Total CPU time | **77.05 s** | 170.68 s | 83.35 s | 169.37 s |
| Peak sampled process-tree PSS | 815 MiB | 1,286 MiB | **732 MiB** | 1,193 MiB |
| Filesystem output blocks | **562,360** | 1,068,256 | 641,256 | 1,170,048 |
| Store file lengths | 160.5 MiB | 160.5 MiB | 161.0 MiB | **58.3 MiB** |

The rows are medians of three independent cold trials. Jato 0.5 recorded 39 compiler misses and 39 verified hits in each pair. The median with 0.4 was 68 misses and ten hits. kache used less sampled memory and sccache retained a smaller local store. [Cold ripgrep samples](concurrent-ripgrep-0.5.json)

Two cold regex checkouts finished in 8.84 s with Jato 0.5, 10.21 s with 0.4, 8.95 s with kache and 8.18 s with sccache. Jato used 24.54 CPU-seconds and sccache used 50.31. These two-checkout comparisons used commit `3053414`. The final release additionally separates competing edits using fingerprints learned from previous compilations. [Two-checkout regex samples](concurrent-regex-0.5.json)

| Four cold regex release builds, two jobs per checkout | Jato 0.5 | Jato 0.4 | kache 2.2 | sccache 0.19 |
| --- | ---: | ---: | ---: | ---: |
| Wall time to all four executables | 15.32 s | 33.43 s | 15.32 s | 22.80 s |
| Total CPU time | 24.65 s | 108.70 s | 25.09 s | 111.13 s |
| Peak sampled process-tree PSS | 417 MiB | 1,271 MiB | 384 MiB | 1,260 MiB |
| Filesystem output blocks | 113,032 | 433,792 | 119,904 | 509,000 |

The installed release compiled a median 6 units and restored 18 across the four targets. Jato 0.4 compiled 24. All four programs produced the expected output. [Four-checkout trials on the installed release](four-worktrees-0.5.json)

Wall time runs from launching the first Cargo to completion of the last. CPU seconds are summed across the entire trial’s children, including server startup, shutdown, verification commands, and compilers launched by the daemons. The harness runs each trial in a fresh Linux child subreaper. It samples combined proportional set size from those process trees every 200 ms during the build. A sample can miss a short memory peak. Store file lengths include SQLite or daemon caches. Btrfs compression and extent sharing affect physical disk usage.

## Returning to familiar checkouts

Each contender first builds ripgrep 14.1.1 from its locked sources. The subsequent trials clear the physical targets and alternate between the same two physical source trees. Contender order rotates. Jato verifies its inputs on each hit and retains a bounded set of directory checks. kache and sccache use their local daemons. The executable’s version output is checked after every build.

| Warm ripgrep release, three trials | Jato 0.5 | Jato 0.4 | kache 2.2 | sccache 0.19 with base directories |
| --- | ---: | ---: | ---: | ---: |
| Original checkout and clean target | 235 ms | 286 ms | 358 ms | 5.75 s |
| Original checkout and new target | 210 ms | 225 ms | 373 ms | 19.05 s |
| Other checkout and empty target | **271 ms** | 296 ms | 384 ms | 16.87 s |

At the first visit to the second checkout, both Jato versions ran two rustc discovery probes. On returning to either checkout, 0.5 used its previously verified layouts. Switching back in 0.4 ran two probes again. The cold priming builds took 30.69 s with Jato 0.5, 27.28 s with 0.4, 28.75 s with kache and 22.06 s with sccache. Cold priming is a single observation per contender. [Three-trial controls at commit 3053414](worktree-revisits-0.5.json)

In a further seven-trial comparison using the installed binary, returning to the second checkout took 234 ms with Jato 0.5, 320 ms with 0.4, and 379 ms with kache. A final two-checkout cold ripgrep run recorded 39 eligible compiler misses and 39 verified hits. Its wall time was 19.96 s. [Installed warm trials](installed-revisits-0.5.json) · [Installed cold confirmation](installed-cold-ripgrep-0.5.json)

An unchanged physical-path control alternated nine warm clean targets per version. Jato 0.5 and 0.4 took 166 ms and 163 ms for check, and 303 ms and 319 ms for release. These control samples used the cache engine before the watch backend change. [Ordinary Cargo path samples](paired-0.5.json)

The packed eight-revision library fixture retained the same 90.9 MiB of compressed payload in both versions. Both restored the initial archive with the same SHA-256. SQLite file lengths increased by 12 KiB across the entire eight-entry fixture. The zlib and embedded-data release smoke tests also executed the restored binaries in automatic and packed modes. [Revision history](history-0.5.json) · [Native archive](zlib-0.5.json) · [Embedded asset](embedded-0.5.json)

## Save to compiler feedback

The two-package fixture adds 4,096 unignored leaf directories in 64 groups. Each version starts three watchers, with five edits per launch. After each break/fix cycle, a populated subtree moves into a member. The harness waits for the workspace to succeed, then edits a file inside the arriving subtree and waits for the next check.

| Median | Jato 0.4 | Jato 0.5 |
| --- | ---: | ---: |
| Launch to filesystem watches ready | 302.0 ms | **56.6 ms** |
| New subtree to full workspace success | 346.0 ms | **70.9 ms** |
| Save to first Rust error | 56.8 ms | 51.2 ms |
| Fix to full workspace success | 58.3 ms | 53.7 ms |
| Idle process-tree PSS | 5,986 KiB | **3,039 KiB** |

Readiness and idle memory use the median across launches. Latencies use all fifteen events. In a separate small workspace, eleven saves produced a median first error in 54.1 ms with ordinary watch and 56.9 ms with projected watch. [Large-tree trials](large-watch-0.5.json) · [Ordinary watch](feedback-0.5.json) · [Projected watch](worktree-feedback-0.5.json)

## Reproduce

Prefetch the locked fixture dependencies. Put the upstream peer binaries on PATH, or pass `--kache` and `--sccache` explicitly. The harnesses accept a disk-backed `--scratch` directory and a path to the earlier Jato executable. The ripgrep workspace comes from the published source archive. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1)

```sh
python3 bench/concurrent.py --previous-jato /path/to/jato-0.4.0 --copies 2 --jobs 4 --runs 3 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg
python3 bench/concurrent.py --previous-jato /path/to/jato-0.4.0 --copies 4 --jobs 2 --runs 3
python3 bench/worktrees.py --tools worktree kache sccache_basedirs --previous-worktree /path/to/jato-0.4.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --revisit --runs 3
python3 bench/feedback.py --directories 4096 --runs 5
python3 bench/feedback.py --runs 11
```
