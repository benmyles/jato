# Working on Jato

- Keep Cargo in charge of dependency resolution, build scripts, linking, testing, and incremental compilation. Exercise changes through real Cargo projects.
- A cache hit must reproduce the compiler's files, diagnostics, and exit status. Delegate unfamiliar compiler invocations to the original command.
- Keep cache I/O bounded. Publish complete entries transactionally and verify restored bytes before notifying Cargo.
- Use an isolated target directory and JATO_CACHE_DIR when testing. Keep machine-specific measurements reproducible.
- Run cargo fmt --check, cargo clippy --all-targets -- -D warnings, and cargo test --locked before release. Keep README.md, TODO.md, and the concise work journal current. Commit working milestones.
- Change the cache-key epoch when interpretation of compiler inputs or artifact provenance changes. Metadata-only evidence must never authorize code generation.
- Use bench/run.py for clean and restored builds and bench/feedback.py for save-to-diagnostic latency. Record workload, profile, repetitions, and machine details with any performance claim.
- Use bench/compare.py for comparisons with other compiler caches. Isolate their daemons, configuration, stores and target directories, and verify the built program. Include cold builds, fresh builds and edits alongside warm restores.
- For native cache changes, run the suite with JATO_TEST_NATIVE_HITS=1 on x86-64 GNU Linux with strace. Validate both shared and per-unit Cargo output layouts. For sealed storage changes, also set JATO_TEST_SEALED=1 on compressed Btrfs with fs-verity. A restored target must remain independently writable.
- A native build using incremental mode needs an observed fresh compiler session before it can publish an entry. Exercise a live edit as well as a clean restore in the default dev profile.
- Verify parallel digests against standard BLAKE3 at partial and power-of-two boundaries. Borrow only available Cargo job tokens and keep the serial path usable.
- Treat archive boundaries as storage hints. Validate whole restored files and check shared chunk and manifest reclamation after eviction or corruption. Use the embedded and zlib fixtures plus bench/history.py when changing these paths.
- Qualify worktree changes with `JATO_TEST_WORKTREE=1` on Linux with working bubblewrap. Exercise shared global targets, independent checkouts, path-valued macros, configuration overrides, signals and watch events. Use `bench/worktrees.py` to time same-target, new-target and copied-checkout builds separately.
- Packed write changes must preserve byte verification when GC runs between preparation and publication. Keep frame metadata and the exact deduplication index bounded and exercise their spill paths.
