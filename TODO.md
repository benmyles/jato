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

## 0.14.1

- [x] Put a build back in another git worktree, in a clone, on another branch and after a commit: what Cargo itself reads of a repository is not an input of a build, what a build script reads there is.
- [x] Let a build outlive the checkout that made it: the directories a workspace is inside are no inputs.
- [x] Take a name for searched for only where the places it was seen in follow one another up from the workspace. A crate kept in `lib` had made every program's look at `/lib` a search, and its build was never found again.

## 0.15

- [x] Start a build of sources that differ a little from the nearest recorded build: its files are put in place without what the compilers wrote whose inputs differ, Cargo makes those again and what depends on them, and the result is recorded like a build from nothing. Who examined each input is part of a whole build's record.
- [x] Do not take a call for missed that a thread never made because its program exited while the call waited to be looked at. One in four checks of all of Olo went unrecorded for it on a busy machine.
- [x] Say in `jato stats` when the last build recorded is of use only in its own target directory: someone who starts using Jato where Cargo has already built gets nothing in the next worktree, and was not told why.
- [x] Read the records that may stand for a command one at a time. All of them were read at once, three times for a build that had to be made: for a workspace of Olo's size that is 90 MB each time once two dozen builds are recorded.
- [x] Record a build made where another command's build already was as resting on it, with that build's files and conditions, so that it is put back in a target directory the other command never used. Clippy first in a new worktree of Olo had been 55 s for one package.
- [x] Take a build that Cargo runs for clippy for Cargo's own: the command's own program is any its first process has been.
- [x] Know text in the target directory that names a root by what it says without the name, as a compiler reads it. A crate that includes generated source with full paths in it was compiled again in every checkout.
- [x] Take the terminal out of what tells one build from another. A person at a terminal was never given a build an agent had made, nor one made in a window of another width, and a crate with a warning was compiled again for each width. Colours are left out where a build is put back without a terminal, as Cargo leaves them out.
- [x] Count the names in a directory Cargo lists only where Cargo goes by them. In a git checkout Cargo lists the top of the repository, and a file git does not track there, or a `target` directory beside a build made elsewhere, kept builds from being put back.

## 0.16

- [x] Give Cargo its tokens in a pipe with a name. With its own, Cargo starts every compiler as a copy of itself: 29 of the 60 processor-seconds of a check of all of Olo in which every compilation but 49 was answered, one compiler at a time. That check went from 15 s to 11 s. Left alone wherever something says how many compilers to run, and where a make older than 4.4 would be found.
- [x] Take the loader's list of directories out of what tells one compiler command from another, and compare it by what would be found. Cargo lists one directory more for what a build script starts in a build than in a check, so the 1,113 commands that Olo's build scripts start, most of its C and assembly, were made again by the first build after a check.
- [x] Put a build back with no more files open than the threads doing it. Every file to copy was opened first, and a check of all of Olo has 13,000 of them: under the usual limit of 1,024 nothing was put back and nothing said so but the log.
- [x] Find what programs keep in the home directory by its name. In a checkout directly in the home directory, git's `.gitconfig` was one step up from the workspace, and every worktree further down looked for it one step up from itself.
- [x] Leave a pipe of tokens that has a name out of what a compiler examined. A build started by make with such a pipe was never recorded.
- [x] Learn which process a thread belongs to with one read of the top of its status. It was read whole, in 15,000 pieces, half of all the system calls of a build in which every compiler was answered.
- [x] `JATO_RECIPES` writes what every command is known by where two runs can be compared.
- [x] Start a build from one made in another checkout whose output names that checkout. The record says which compilers wrote such output, and they are run again. Olo's `build --workspace` in a new worktree: 47 s to 12 s.
- [x] Read a file before compressing it, and do not compress what the cache holds; read a file that has two names once. Recording a dev build of all of Olo on ext4: 20 s to 2.5 s.
- [x] Put a build back with a thread for each processor up to sixteen, each taking the next largest file, give files their time as they are written, and move them into place on several threads. A dev build of all of Olo, 21,600 files and 21 GB on ext4: 4.3 to 5.1 s, now 2.7 to 4.
- [x] Read a build's files into the cache on several threads when it is recorded. A dev build of all of Olo: 2.5 s to 1.1 s on ext4, 4.7 s to 2.3 s on Btrfs.
- [x] Take the time for a build's files before its sources are looked at. A source saved while the files were being written looked older than the build to Cargo.

## Open

- [ ] Putting a dev build of all of Olo back on ext4 takes 3 to 4 s, two or more of them writing 21 GB through memory at the rate memory takes it. On a filesystem that clones files none of it is copied.
- [ ] The `cc` crate names an object file after a hash of the directory its source is in. Source that a build script generates in its output directory is in another directory in every checkout, so 41 of Olo's C compilations are made again in each, 2.4 s for the longest. The result could be kept by what was compiled and given the name asked for.
- [ ] A build script that writes something else every time it runs has everything above it compiled again wherever Cargo runs the script: `grammers-tl-types` generates its source in an order that changes, 3.7 s for the crate and seven more after it. Putting a build script's run back, as recorded, would do away with that and with the scripts' own time.

- [ ] A build can start from an earlier one only when what differs was read by compilers Cargo started. A changed manifest or lock file, a changed input of a build script and changed C that a build script compiles leave Cargo to carry the build out: 22 s for a check of all of Olo. For C, leaving out what Cargo remembers of the build script's run would have Cargo run the script again. For a manifest, the parts whose command lines are unchanged could be found in the earlier build by what they wrote.
- [ ] A build in which a build script was built again is not recorded when it started from an earlier one, because the script ran where its earlier version had written. Clearing what the earlier version wrote first would make the build one that can be recorded.
- [ ] A build rests on another command's build only while that build's inputs are all as they were. Made after an edit, in a target directory the other command had used, it is recorded with what it found there as a condition, and is put back only where the same was done before it.
- [ ] A build script that prints `rerun-if-changed` with a full path is run again by Cargo in every new checkout, after a build is put back and Cargo next runs: Cargo keeps the path in what it remembers of the script. Olo has two, and the first `cargo test` in a new worktree spends 20 s on one of them.
- [ ] A build in a target directory that is already built is left to Cargo and not recorded as a whole, so sources arrived at by editing in one worktree are built again, from the nearest recorded build, by the next worktree that has them. Recording such builds as they are made needs a cheaper record than one that names every input again.


- [ ] What a compiler writes is still reported by the kernel one call at a time, and rustc writes hundreds of files into the directory of its saved state, which Jato does not record. A check of `olo-system` is down from about 2,600 reported calls to 160; a full build of a crate has many more. The library in watched programs could make those calls unreported for directories Jato names to it, if the filter let more than reads through from its address.
- [ ] With programs telling of their own reads, a compilation of `olo-system` watched and unwatched in alternation at a load of 38 to 46 took the same time within what runs differ by (8 to 17 s for 7 processor-seconds). Whether compilations in a built target still need to go unwatched on a busy machine wants the same comparison for the edits of the whole workspace, side by side.
- [ ] The library for watched programs exists for x86-64. On aarch64 every call is reported by the kernel.

- [ ] rustc generates all of a program's code again in a target directory other than the one its state was saved in, and Jato cannot hand state for a program to a target directory outside the workspace at all. rustc goes by the real location of every library a program links, whatever name it is given, and compares its options with the real workspace path in them. Compiling inside a mount namespace of the build's own, in which the workspace and the target directory are always at the same two paths, would make all of rustc's state fit everywhere, and would do away with the other names for directories, the adjustment of the state's option bytes and results that only suit one checkout. It needs unprivileged user namespaces, which some systems and most containers withhold, so the present way would have to stay beside it.
- [ ] Where a whole build does not apply, Cargo runs every build script again in a target directory that has none of their output, even those whose inputs are as they were: 113 of them on Olo, about 11 processor-seconds and most of the minute such a build takes. A build script's run could be recorded as a part and its output put back before Cargo looks, with what Cargo remembers of it. Cargo would have to find that as it expects, which for a build script that names no inputs includes the time of the newest file in the package.
- [ ] A build script that names no inputs is run again by Cargo after a whole build is put back in another checkout, when Cargo is started at all: Cargo remembers the package by the time and path of its newest file.
- [ ] A search through the directories above a workspace that finds something is taken to have found that very file: a workspace is given the result only if its own search finds the same one. A copy of a whole tree somewhere else, with a file of the same contents in the same place above it, compiles again. Comparing what is found by how far up it is, and by what it holds, would let such copies share, and has to cope with a file that is found from different depths.
- [ ] A directory opened for reading counts as listed, since the calls that read its entries are not reported. A program that opens one only to make its contents durable, as a database does, makes what the directory holds an input all the same. Cargo's own such files are left out of listings by name.
- [ ] A crate from beside the workspace gets a different name from Cargo in every checkout, so its compilations and those of its dependents are not shared between checkouts, only whole builds are.
- [ ] rustc's incremental state doubles the size of a build made from scratch, in the target directory and in the cache. Leaving it out of such builds would need another source for the state handed on at the first edit: on Olo, compiling a crate without it was no cheaper than compiling it incrementally from nothing (116 to 127 against about 110 processor-seconds), so nothing is gained by leaving it out of first edits.
- [ ] Reuse test results: a test program whose own file and everything it read are unchanged could report what it reported before. It needs a rule for the environment, which a program reads unseen, and for tests that depend on time or chance.
- [ ] Finding that nothing of a check of all of Olo needs putting back takes 0.2 s, most of it asking about each of the 69,000 inputs one at a time.
- [ ] Evict by what a result costs to rebuild and how much room it takes, not only by when it was last used. A linked program is large and quick to make again; a library is the reverse.
- [ ] One jobserver for all builds on the machine. The limit on running compilers bounds processes, not the threads each compiler starts with tokens from its own build.
- [ ] Recording on macOS and Windows. Commands there run as they would without Jato.
- [ ] Notice a crate Cargo believes is up to date although a file only the compiler examined has changed, and have it compiled again. Whole builds already see this; a build Cargo carries out itself does not.
- [ ] Record the builds of `jato test --doc`, and of a target directory set only in a Cargo configuration file, relative to that directory.
- [ ] Say why a build was not reused in one line without `JATO_LOG`, on request: `jato why`.
- [ ] The names `cc`, `gcc` and the others stand for Jato only where a build script finds its compiler along `PATH`. One named by its full path, in `CC` or by CMake's own search, is run as it is and its work is the build script's. A compiler other than gcc and clang is not told the workspace's other names.
