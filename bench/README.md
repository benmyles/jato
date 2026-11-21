# Jato 0.3.0 measurements

The comparison uses the installed Jato 0.3.0, Jato 0.2.0, the machine's kache 2.1.1, upstream kache 2.2.0, upstream sccache 0.19.0 and plain Cargo. All timed builds use the same Rust 1.100.0 toolchain, four Cargo jobs and default profile settings on an Intel Core Ultra 7 356H running Fedora 45. The local Btrfs volume uses Zstandard level 1 compression. The workstation was shared with other workloads. Full per-run timings and counters accompany the medians. [0.2 report](0.2.md) · [0.1 report](0.1.md)

## Reproduce

Install Jato from this repository. Save a separate 0.2.0 binary to repeat the version comparison. The harness resolves the stock toolchain, uses private stores and peer daemon sockets, and exercises the normal Cargo compiler-wrapper interface. It uses the same absolute target path for each contender. Each tool, profile and trial starts with an empty store. The first build fills it, the warm build starts from a second empty target, and the fresh build retains that target. Tool order rotates and profile order alternates between trials.

```sh
python3 bench/compare.py --project /path/to/ripgrep-14.1.1 --verify-bin rg --previous-jato /path/to/jato-0.2.0 --include-packed --previous-packed --runs 3 --jobs 4
python3 bench/compare.py --fixture regex --previous-jato /path/to/jato-0.2.0 --include-packed --runs 3 --jobs 4
python3 bench/compare.py --fixture zlib --previous-jato /path/to/jato-0.2.0 --runs 3 --jobs 4
python3 bench/compare.py --fixture embedded --previous-jato /path/to/jato-0.2.0 --runs 3 --jobs 4
python3 bench/compare.py --project /path/to/ripgrep-14.1.1 --workloads check --previous-jato /path/to/jato-0.2.0 --paired-restores 21 --jobs 4
python3 bench/compare.py --tools jato kache --kache /path/to/kache-2.2.0/kache --project /path/to/ripgrep-14.1.1 --verify-bin rg --runs 3 --jobs 4
python3 bench/history.py --jato jato --previous-jato /path/to/jato-0.2.0 --variants 8
python3 bench/feedback.py --jato jato --runs 11
```

Use `--scratch` to select a filesystem. The comparison accepts `--offline` when the locked dependencies have already been fetched. Benchmark setup, target deletion, executable checks and daemon startup or shutdown are outside the timer. Clean refers to the target directory. The OS page cache stays enabled.

- The external project is the published ripgrep 14.1.1 crate with SHA-256 `f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9`. The harness compares the release executable's version output after every build. [ripgrep source](https://crates.io/crates/ripgrep/14.1.1)
- The regex fixture checks a short expression and counts three matches. The zlib fixture uses flate2 and libz-sys to compress and decompress a message with the system library. Install zlib's development files and pkg-config to reproduce the system-library path. [Regex fixture](regex-app/) · [Zlib fixture](zlib-app/)
- The embedded-data fixture generates 12 MiB plus 17 bytes of deterministic data before timing. Its compiled program reads a runtime-selected byte. Each built-in fixture changes one line and undoes the edit after an unchanged check. [Embedded fixture](embedded-app/)
- The storage-history workload changes an exported symbol in a Rust library across eight revisions while retaining a 4 MiB data table. It builds the library in release mode, records packed storage after each revision, and verifies the SHA-256 of the restored first archive. [History harness](history.py)
- The feedback workload times a real compiler error after editing a workspace member, then times the successful workspace follow-up after fixing it. It also samples the idle watcher's resident memory. [Feedback harness](feedback.py)

Store bytes are apparent file lengths counted once per inode. Btrfs can compress data and share extents with the target directory. The harness flushes each private store after timing for the separate extent measurements. GNU time reports the largest process in the timed command tree. Peer servers and the compiler children they launch can run outside that tree. The comparison also records the private listening server's idle RSS. Jato's build cache exits after each build.

The released Jato executable's SHA-256 is `75ae82f142ef93b6a73d4ca6e464ed50c1ace3ac873106a62ae8c3e1dcb10711`. The comparison used the same kache, kache-engine, sccache and previous Jato executables recorded in the [0.2 measurements](0.2.md). The kernel was 7.3.4 and pkg-config reported system zlib `1.3.1.zlib-ng`.

The additional kache 2.2.0 comparison uses the published x86-64 Linux musl archive with SHA-256 `eeb6e72b9f55d0e9d61cae0789ffa8f3ab8c33bef166b7af655cc0806447ebac`, including its matching engine. Use `--kache` to select it and `--local-kache` to include another installed kache in the same fixture comparison. [kache 2.2.0 release](https://github.com/kunobi-ninja/kache/releases/tag/v2.2.0)
