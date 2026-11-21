# Measuring Rust compiler caches

The current benchmark compares Jato 0.2, kache, sccache and plain Cargo on the same machine. It measures the wall time a developer waits, including Cargo startup and input validation. See the [Jato 0.1 benchmarks](0.1.md) for the original baseline.

## Reproduce the comparison

Use Linux, GNU time, a C toolchain, strace and release builds of the three cache tools. The kache executable must support private local daemon sockets. The harness creates private Cargo configuration, caches and peer daemons, selects the stock toolchain and system C driver, and runs all contenders locally. It reuses downloaded registry sources. A run can also compare a saved older Jato binary and the portable packed store.

```sh
python3 bench/compare.py --jato jato --kache kache --sccache sccache --runs 3 --jobs 4 --include-packed
python3 bench/compare.py --project /path/to/ripgrep-14.1.1 --verify-bin rg --runs 3 --jobs 4
python3 bench/feedback.py --jato jato --runs 9
```

All project builds share the same absolute target path and Cargo arguments. Each tool, profile and trial gets an empty private store. The cold build begins with an empty target. The warm build uses the same store and another empty target. The fresh build retains the target. For the locked regex fixture, a one-line change and its undo follow the fresh check. The harness executes each release binary, checks the fixture's answer, and compares ripgrep's version output across tools. Trial order rotates between tools and reverses between profiles. Peer daemon startup and shutdown happen outside the timer. Use `--offline` after fetching dependencies and `--scratch` to choose a disk for temporary files.

Each timing is in seconds in the raw comparison data. GNU time reports the maximum resident size of a process in the timed command tree. A peer server and compiler children it launches independently can live outside this tree. The harness measures the idle resident size of the private listening server separately. The Jato build cache exits after the build. The optional file watcher has its own measured idle footprint.

Store bytes count file lengths once per inode in the private store, including database files after daemon shutdown. Those lengths include extents shared with Cargo's target. Btrfs extent samples depend on writeback and sharing, and filesystem compression also affects physical use. Use the packed-mode comparison for the trade-off between file lengths and restore latency.

The ripgrep input is the published 14.1.1 crate, SHA-256 `f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9`. The regex input and its dependency lockfile ship with Jato. External projects retain their sources during the benchmark. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1) · [regex fixture](regex-app/)
