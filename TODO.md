# Jato checklist

## The 0.12 engine

- [x] Replace admission rules and compiler-specific keys with observed command results: what a command examined is recorded as it runs, and a result is reused when all of it is unchanged.
- [x] Store results relative to the workspace and target directory, so another target directory or checkout finds them. Keep output that names a real location where it was made.
- [x] Record and restore whole Cargo commands. `jato build`, `check` and `clippy` are looked up as one result; `jato test`, `bench` and `run` have the build they need looked up or made first.
- [x] Keep edit builds at Cargo's speed: a crate compiled again within minutes is left to rustc's incremental compilation, unwatched and unrecorded.
- [x] Share the machine among builds: compilers started through Jato queue for as many places as there are processors.
- [x] Make the first edit in a restored target incremental: keep the state rustc saves, once for each crate, and hand it to a compilation that has none, in any checkout, and for libraries in any target directory.
- [x] Step around another compiler cache that is set up on the machine: its directory of stand-ins on PATH, C compilers named behind it, rustdoc and rustc named as links to it.

## 0.13

- [x] Remember the C, C++ and assembly that build scripts compile, like Rust: looked up, observed and recorded as parts of the build, with the workspace's other names passed to gcc and clang so that their output does not keep a build with one checkout.
- [x] Count a part that cannot be recorded, or that failed without failing the build, as the build's own doing.
- [x] Look at what a build takes from beside its checkout beside each checkout, and at what a search above it finds above each.
- [x] Give restored files a time that an edit made right afterwards is certain to be newer than, and leave a lock file that is already right alone.
- [x] Say which program and which file kept a build from being recorded or with one checkout.
- [x] Leave compilations in a built target unwatched while every processor is in use.

## 0.14

- [x] Have watched programs tell of the files they read themselves, through a library loaded ahead of their C library, with the kernel's filter letting only those calls through unreported and reporting everything else as before.
- [x] Take a directory's listing down when it is opened for reading, and stop having the calls that read its entries reported.
- [x] Turn a record down when a path the command found missing is there by the time it ends.
- [x] Keep objects as clones of compiler output where the filesystem allows, compressed copies elsewhere, under one budget with least-recently-used eviction.
- [x] Test through real Cargo projects: restores, edits and undo, other checkouts and targets, proc macros that read files and directories, build scripts, linkers, damaged and evicted objects, concurrent builds, the network, PATH changes.
- [x] Compare with kache, sccache and bare Cargo on ripgrep and Olo, with private stores and daemons.

## Open

- [ ] What a compiler writes is still reported by the kernel one call at a time, and rustc writes hundreds of files into the directory of its saved state, which Jato does not record. A check of `olo-system` is down from about 2,600 reported calls to 160; a full build of a crate has many more. The library in watched programs could make those calls unreported for directories Jato names to it, if the filter let more than reads through from its address.
- [ ] With programs telling of their own reads, whether compilations in a built target still need to go unwatched on a busy machine has to be measured again at a load of 45 and more.
- [ ] The library for watched programs exists for x86-64. On aarch64 every call is reported by the kernel.

- [ ] rustc generates all of a program's code again in a target directory other than the one its state was saved in, and Jato cannot hand state for a program to a target directory outside the workspace at all. rustc goes by the real location of every library a program links, whatever name it is given, and compares its options with the real workspace path in them. Compiling inside a mount namespace of the build's own, in which the workspace and the target directory are always at the same two paths, would make all of rustc's state fit everywhere, and would do away with the other names for directories, the adjustment of the state's option bytes and results that only suit one checkout. It needs unprivileged user namespaces, which some systems and most containers withhold, so the present way would have to stay beside it.
- [ ] Where a whole build does not apply, Cargo runs every build script again in a target directory that has none of their output, even those whose inputs are as they were: 113 of them on Olo, about 11 processor-seconds and most of the minute such a build takes. A build script's run could be recorded as a part and its output put back before Cargo looks, with what Cargo remembers of it. Cargo would have to find that as it expects, which for a build script that names no inputs includes the time of the newest file in the package.
- [ ] A build script that names no inputs is run again by Cargo after a whole build is put back in another checkout, when Cargo is started at all: Cargo remembers the package by the time and path of its newest file.
- [ ] A search through the directories above a workspace that finds something is taken to have found that very file: a workspace is given the result only if its own search finds the same one. A copy of a whole tree somewhere else, with a file of the same contents in the same place above it, compiles again. Comparing what is found by how far up it is, and by what it holds, would let such copies share, and has to cope with a file that is found from different depths.
- [ ] A directory opened for reading counts as listed, since the calls that read its entries are not reported. A program that opens one only to make its contents durable, as a database does, makes what the directory holds an input all the same. Cargo's own such files are left out of listings by name.
- [ ] A crate from beside the workspace gets a different name from Cargo in every checkout, so its compilations and those of its dependents are not shared between checkouts, only whole builds are.
- [ ] rustc's incremental state doubles the size of a build made from scratch, in the target directory and in the cache. Leaving it out of such builds would need another source for the state handed on at the first edit: on Olo, compiling a crate without it was no cheaper than compiling it incrementally from nothing (116 to 127 against about 110 processor-seconds), so nothing is gained by leaving it out of first edits.
- [ ] Reuse test results: a test program whose own file and everything it read are unchanged could report what it reported before. It needs a rule for the environment, which a program reads unseen, and for tests that depend on time or chance.
- [ ] Putting back the 13,000 files of a check of all of Olo takes 1.4 to 2.5 s, and finding that nothing needs putting back 0.3 s. Each file costs about ten system calls, and each of the 82,000 inputs is asked about one at a time.
- [ ] Evict by what a result costs to rebuild and how much room it takes, not only by when it was last used. A linked program is large and quick to make again; a library is the reverse.
- [ ] One jobserver for all builds on the machine. The limit on running compilers bounds processes, not the threads each compiler starts with tokens from its own build.
- [ ] Recording on macOS and Windows. Commands there run as they would without Jato.
- [ ] Notice a crate Cargo believes is up to date although a file only the compiler examined has changed, and have it compiled again. Whole builds already see this; a build Cargo carries out itself does not.
- [ ] Record the builds of `jato test --doc`, and of a target directory set only in a Cargo configuration file, relative to that directory.
- [ ] Say why a build was not reused in one line without `JATO_LOG`, on request: `jato why`.
- [ ] The names `cc`, `gcc` and the others stand for Jato only where a build script finds its compiler along `PATH`. One named by its full path, in `CC` or by CMake's own search, is run as it is and its work is the build script's. A compiler other than gcc and clang is not told the workspace's other names.
