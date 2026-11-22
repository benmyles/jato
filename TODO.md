# Jato checklist

## The 0.11 engine

- [x] Replace admission rules and compiler-specific keys with observed command results: what a command examined is recorded as it runs, and a result is reused when all of it is unchanged.
- [x] Store results relative to the workspace and target directory, so another target directory or checkout finds them. Keep output that names a real location where it was made.
- [x] Record and restore whole Cargo commands. `jato build`, `check` and `clippy` are looked up as one result; `jato test`, `bench` and `run` have the build they need looked up or made first.
- [x] Keep edit builds at Cargo's speed: a crate compiled again within minutes is left to rustc's incremental compilation, unwatched and unrecorded.
- [x] Share the machine among builds: compilers started through Jato queue for as many places as there are processors.
- [x] Make the first edit in a restored target incremental: keep rustc's saved state with a recorded compilation and hand it to a compilation that has none, in any checkout.
- [x] Keep objects as clones of compiler output where the filesystem allows, compressed copies elsewhere, under one budget with least-recently-used eviction.
- [x] Test through real Cargo projects: restores, edits and undo, other checkouts and targets, proc macros that read files and directories, build scripts, linkers, damaged and evicted objects, concurrent builds, the network, PATH changes.
- [x] Compare with kache, sccache and bare Cargo on ripgrep and Olo, with private stores and daemons.

## Open

- [ ] Saved state follows a build only when the target directory lies inside the workspace, where the output directory can be named the same in every checkout. With a target directory elsewhere rustc reuses its analysis and generates all code again. An option to rustc that left real paths out of what it compares would make the adjustment of its state unnecessary; `-Zremap-cwd-prefix` does not, since it too records the real directory.
- [ ] Leave rustc's incremental state out of a build that is being recorded from scratch, as kache does. It is as large as the build's outputs, and with saved state handed on the first edit no longer needs it to be there.
- [ ] Reuse test results: a test program whose own file and everything it read are unchanged could report what it reported before. It needs a rule for the environment, which a program reads unseen, and for tests that depend on time or chance.
- [ ] Evict by what a result costs to rebuild and how much room it takes, not only by when it was last used. A linked program is large and quick to make again; a library is the reverse.
- [ ] One jobserver for all builds on the machine. The limit on running compilers bounds processes, not the threads each compiler starts with tokens from its own build.
- [ ] Recording on macOS and Windows. Commands there run as they would without Jato.
- [ ] Notice a crate Cargo believes is up to date although a file only the compiler examined has changed, and have it compiled again. Whole builds already see this; a build Cargo carries out itself does not.
- [ ] Record the builds of `jato test --doc`, and of a target directory set only in a Cargo configuration file, relative to that directory.
- [ ] Say why a build was not reused in one line without `JATO_LOG`, on request: `jato why`.
- [ ] C that a build script compiles from the workspace carries the workspace's location in its debug information, which keeps the whole build with that checkout. Pass the compiler the same aliases rustc gets.
