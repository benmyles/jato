# Working on Jato

- Keep Cargo in charge of dependency resolution, build scripts, linking, testing, and incremental compilation. Exercise changes through real Cargo projects.
- A cache hit must reproduce the compiler's files, diagnostics, and exit status. Delegate unfamiliar compiler invocations to the original command.
- Keep cache I/O bounded. Publish complete entries transactionally and verify restored bytes before notifying Cargo.
- Use an isolated target directory and JATO_CACHE_DIR when testing. Keep machine-specific measurements reproducible.
- Run cargo fmt --check, cargo clippy --all-targets -- -D warnings, and cargo test --locked before release. Keep README.md, TODO.md, and the concise work journal current. Commit working milestones.
