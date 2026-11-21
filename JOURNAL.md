# Work journal

## 2026-11-21

- Started from an empty repository. Found a locally customized kache installation and checked its scheduler notes and regression scenarios.
- Chose a local, daemon-free cache with content-addressed chunks and a separate edit watcher. Cargo keeps its normal target directory, scheduling, and incremental state.
- Selected cold, restored, unchanged, and edited builds as distinct measurement cases.
- Built and tested transactional storage, 256 KiB compressed chunks, duplicate elimination, staged restores, corruption recovery, and LRU pruning. Two storage regression tests pass.
- Integrated Cargo and rustc. Clean builds now restore libraries and check metadata, including empty metadata. Compiler errors, Cargo JSON diagnostics, test arguments, and ordinary incremental builds survive the wrapper.
- Exercised source edits, generated input paths containing spaces, environment changes, A→B→A reuse, new ambiguous modules, and an unusable cache in isolated Cargo projects.
