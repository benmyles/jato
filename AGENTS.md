# Working on Jato

- Keep Cargo in charge of dependency resolution, build scripts, linking, testing, and incremental compilation. Exercise changes through real Cargo projects.
- A cache hit must reproduce the compiler's files, diagnostics, and exit status. Delegate unfamiliar compiler invocations to the original command.
- Keep cache I/O bounded. Publish complete entries transactionally and verify restored bytes before notifying Cargo.
- Use an isolated target directory and JATO_CACHE_DIR when testing. Keep machine-specific measurements reproducible.
- Run cargo fmt --check, cargo clippy --all-targets -- -D warnings, and cargo test --locked before release. Keep README.md, TODO.md, and the concise work journal current. Commit working milestones.
- Change the cache-key epoch when interpretation of compiler inputs or artifact provenance changes. Metadata-only evidence must never authorize code generation.
- Use bench/run.py for clean and restored builds and bench/feedback.py for save-to-diagnostic latency. Record workload, profile, repetitions, and machine details with any performance claim.
