//! rustc dep-info describes Rust inputs. Assembly and native link attributes
//! can open additional files behind rustc's back. Inspect the successful
//! compilation's input set once, before admitting any of its artifacts.
use crate::invocation::Invocation;
use anyhow::{Result, ensure};
use memchr::memmem;
use rustc_lexer::{TokenKind, tokenize};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

pub fn verify<'a>(
    files: impl Iterator<Item = &'a PathBuf>,
    inv: &Invocation,
    compiler: &OsStr,
) -> Result<()> {
    // Stable rejects active feature gates. Documentation-only cfg_attr gates
    // are common and do not make a successful stable compilation opaque.
    let version = Command::new(compiler).arg("-V").output()?;
    let output = std::str::from_utf8(&version.stdout)?;
    ensure!(
        version.status.success()
            && output
                .strip_prefix("rustc ")
                .and_then(|v| v.split_whitespace().next())
                .is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit() || c == b'.')),
        "prerelease or custom compiler"
    );
    const MAX: u64 = 8 << 20;
    let mut bytes = Vec::new();
    for path in files {
        let f = File::open(path)?;
        ensure!(
            f.metadata()?.len() <= MAX,
            "source exceeds admission scan limit"
        );
        bytes.clear();
        f.take(MAX + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX as usize,
            "source changed size during admission"
        );
        if [b"asm".as_slice(), b"link", b"extern"]
            .iter()
            .any(|p| memmem::find(&bytes, p).is_some())
            && let Ok(src) = std::str::from_utf8(&bytes)
            && let Some(reason) = opaque_input(src, inv)
        {
            anyhow::bail!("{reason} in {}", path.display());
        }
    }
    Ok(())
}

fn opaque_input(source: &str, inv: &Invocation) -> Option<&'static str> {
    let mut input = source;
    let mut prev = "";
    let mut before = "";
    let mut sensitive = Vec::new();
    let mut macro_definition = false;
    for token in tokenize(source) {
        let (spelling, rest) = input.split_at(token.len);
        input = rest;
        use TokenKind::*;
        if matches!(token.kind, Whitespace | LineComment | BlockComment { .. }) {
            continue;
        }
        let word = spelling.strip_prefix("r#").unwrap_or(spelling);
        if before == "extern" && prev == "crate" && !inv.has_extern(word) {
            return Some("undeclared extern");
        }
        match token.kind {
            Ident | RawIdent if matches!(word, "asm" | "global_asm" | "llvm_asm" | "naked_asm") => {
                return Some("assembly input");
            }
            Ident | RawIdent
                if word == "link" && prev != "." && sensitive.last() == Some(&true) =>
            {
                return Some("native link attribute");
            }
            Ident if word == "macro_rules" => macro_definition = true,
            OpenBrace | OpenBracket | OpenParen => {
                let group = macro_definition
                    || matches!(prev, "#" | "!")
                    || sensitive.last() == Some(&true);
                sensitive.push(group);
                macro_definition = false;
            }
            CloseParen | CloseBrace | CloseBracket => {
                sensitive.pop();
            }
            _ => (),
        }
        before = prev;
        prev = word;
    }
    None
}
