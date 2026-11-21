# Jato

Jato is a local Rust build cache and workspace edit runner. It targets repeated Cargo builds after cleans or branch changes and shortens the time between a save and compiler feedback.

## Design notes

- Cargo already skips fresh work and maintains incremental compiler state. Jato wraps individual compiler calls and leaves Cargo's scheduling and profile settings in place. [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)
- Rust compilation can depend on generated sources, compile-time environment variables, procedural macros, and native libraries. Jato uses compiler dependency information and takes the ordinary compiler path for opaque inputs. [sccache Rust support](https://github.com/mozilla/sccache/blob/main/docs/Rust.md)
- Compressed content addressing and size-based eviction limit persistent storage. Restored outputs must be independently writable. [ccache manual](https://ccache.dev/manual/latest.html) · [kache](https://github.com/kunobi-ninja/kache)
- Whole-directory CI archives and interactive watch tools serve different workloads. Jato stores individual compiler outputs and uses workspace package ownership to give a developer earlier feedback on edits. [rust-cache](https://github.com/Swatinem/rust-cache) · [Bacon](https://dystroy.org/bacon/) · [Cargo Watch](https://github.com/watchexec/cargo-watch)
