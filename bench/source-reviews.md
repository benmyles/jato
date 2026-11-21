# Rust source review for code generation

Rustc's dep-info describes the Rust inputs to a successful compile. Native assembly can also read files. Four Linux library source sets use declarative macros or namespace aliases that the ordinary syntax check cannot fully resolve. The following review applies to the exact sources and bindings in the published receipts. [Receipt data, including archive SHA-256 and configuration](source-receipts-0.11.json)

| Library | Active files | Reviewed use |
| --- | ---: | --- |
| libc 0.2.191 | 65 | The private `cfg_if!` in `src/macros.rs` forwards complete Rust items. In `src/new/mod.rs`, `pub use asm::socket::*` refers to a Rust module. |
| cmov 0.5.4 | 7 | The two local macros in `src/backends/x86.rs` take register operands. Every substitution for an instruction is a fixed test, XOR or conditional-move template. |
| num-bigint 0.4.8 | 28 | The local `cfg_digit!` forwards one of two item definitions. In `src/biguint/division.rs` each possible local `div!()` expands to a single literal division instruction. |
| rustix 1.1.5 | 180 | `asm` is a module name in the Linux syscall dispatch macros. The x86-64 implementation emits literal `syscall` and `ud2` instructions. The 32-bit vDSO assembler block is cfg-disabled on the qualified target and has a fixed, file-independent literal. |

A macro can interpret its argument as tokens, and a consumer can expand an exported macro after the producer has finished compiling. Reviewing a substring, a filename or a registry version would leave its meaning unresolved. [Rust macros by example](https://doc.rust-lang.org/reference/macros-by-example.html) · [Inline assembly](https://doc.rust-lang.org/reference/inline-assembly.html)

During publication Jato hashes the logical path, resolved path and full contents of every active file, together with explicit cfgs, edition, target settings and positional extern bindings. The source check still examines native link attributes in the reviewed files. Other files undergo the ordinary assembly scan. An edited file, an added cfg, a redirected input or a rebound extern fails the receipt. Additional source sets can still qualify through ordinary syntax checks. The kernel and compiler toolchain remain part of the normal recipe.

For the two libraries with external providers, Jato saves a checksummed source-origin record on ordinary eligible producer compilations. The record binds the source receipt to the actual content digests rustc consumed. Before granting an assembly exception, it follows those digests through complete code-generation records to the reviewed leaves. It checks `bitflags → serde_core`, `linux-raw-sys`, `num-integer → num-traits → libm`, and `num-traits → libm` as appropriate. A producer with a pending requirement or only a metadata-check record cannot prove an edge. The bounded verification reads at most 24 producer manifests. Serde's single generated module is bound to its exact bytes and the compiler-tracked output directory. No user-installed list grants additional trust.

To reproduce a receipt, capture the original rustc argument vector as a JSON array, excluding the compiler executable, and retain rustc's `.d` file. Run the script from the original compiler working directory. It authenticates the `.crate` archive against its Cargo.lock checksum and compares every active registry file to its archive member. Python 3.11+ and the standard `b3sum` command are required.

```sh
python3 /path/to/jato/bench/source_review.py --archive /path/to/libc-0.2.191.crate \
    --lock /path/to/Cargo.lock --dep-info /path/to/libc.d --rustc-args /path/to/args.json \
    --crate-root /path/to/libc-0.2.191/src/lib.rs
```

The end-to-end qualification copies crate archives into disposable directories, changes a macro-defining file, supplies an additional cfg and changes an external provider while preserving its package name. It verifies both Cargo's result and Jato's cache decisions. [Qualification harness](reviewed_sources.py)
