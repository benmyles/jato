# Jato 0.12 measurements

MACHINE

Each contender had its own empty store, its own daemon where it has one, its own copy of the project and its own target directories. The built program was run after every step and had to print the same thing for every contender. Times are wall-clock seconds.

RESULTS

## Reproduction

Use prefetched dependencies and a scratch directory on disk. `--no-rebuilds cargo` leaves out the steps in which bare Cargo would only repeat its cold build.

```sh
RUSTUP_TOOLCHAIN=stable python3 bench/compare.py --offline --project /path/to/ripgrep-14.1.1 \
    --edit crates/core/main.rs --verify "rg --version" \
    --tools cargo jato jato-wrapper kache sccache --runs 3
python3 bench/compare.py --offline --project /path/to/olo --package olo-system \
    --edit crates/system/src/lib.rs --verify "nm-fake --help" \
    --tools cargo jato kache --no-rebuilds cargo --runs 1
RUSTUP_TOOLCHAIN=stable python3 bench/concurrent.py --offline --project /path/to/ripgrep-14.1.1 \
    --verify "rg --version" --builds 8 --tools cargo jato jato-unlimited kache
python3 bench/feedback.py --jato /path/to/jato
```

`compare.py` takes each contender through a cold build, the same build again, the target directory removed, a new target directory, a copy of the sources at another path, edits to one file and their undoing. `jato` is Jato run as `jato build`; `jato-wrapper` is Jato named as `RUSTC_WRAPPER` under plain Cargo, which reuses single compilations only. `concurrent.py` starts several builds at once that share nothing, to show how they share the machine. `feedback.py` measures a save to the compiler's diagnostic under `jato watch`.
