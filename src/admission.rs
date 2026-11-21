//! rustc dep-info describes Rust inputs. Assembly and native link attributes
//! can open additional files behind rustc's back. Inspect the successful
//! compilation's input set once, before admitting any of its artifacts.
use anyhow::{Result, ensure};
use memchr::memmem;
use rustc_lexer::{TokenKind, tokenize};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

pub fn stable_compiler(compiler: &OsStr) -> Result<bool> {
    // Stable rejects active feature gates. Documentation-only cfg_attr gates
    // are common and do not make a successful stable compilation opaque.
    let version = Command::new(compiler).arg("-V").output()?;
    let output = std::str::from_utf8(&version.stdout)?;
    ensure!(version.status.success(), "could not identify rustc version");
    Ok(output
        .strip_prefix("rustc ")
        .and_then(|v| v.split_whitespace().next())
        .is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit() || c == b'.')))
}

pub fn verify_bytes(bytes: &[u8], path: &Path, deferred_links: bool) -> Result<()> {
    if [b"asm".as_slice(), b"link"]
        .iter()
        .any(|p| memmem::find(bytes, p).is_some())
        && let Ok(src) = std::str::from_utf8(bytes)
        && let Some(reason) = opaque_input(src, deferred_links)
    {
        anyhow::bail!("{reason} in {}", path.display());
    }
    Ok(())
}

fn opaque_input(source: &str, deferred_links: bool) -> Option<&'static str> {
    let mut input = source;
    let mut prev = "";
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
        match token.kind {
            Ident | RawIdent if matches!(word, "asm" | "global_asm" | "llvm_asm" | "naked_asm") => {
                return Some("assembly input");
            }
            Ident | RawIdent
                if word == "link" && prev != "." && sensitive.last() == Some(&true) =>
            {
                if !deferred_links || !matches!(prev, "[" | ",") || !unbundled_link(input) {
                    return Some("native link attribute");
                }
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
        prev = word;
    }
    None
}

// An rlib records dynamic libraries and explicitly unbundled static libraries
// by name. Only final linking opens them. Require literal, unambiguous fields
// and continue to decline bundled archives and synthesized attributes.
fn unbundled_link(input: &str) -> bool {
    fn parse(input: &str) -> Option<bool> {
        use TokenKind::*;
        let mut tail = input;
        let mut tokens = tokenize(input).filter_map(|t| {
            let (word, next) = tail.split_at(t.len);
            tail = next;
            (!matches!(t.kind, Whitespace | LineComment | BlockComment { .. })).then_some(word)
        });
        if tokens.next()? != "(" {
            return None;
        }
        let (mut name, mut kind, mut modifiers) = (None, None, None);
        loop {
            let key = tokens.next()?;
            if key == ")" {
                break;
            }
            if key == "cfg" {
                if tokens.next()? != "(" {
                    return None;
                }
                let mut depth = 1;
                while depth > 0 {
                    match tokens.next()? {
                        "(" => depth += 1,
                        ")" => depth -= 1,
                        "$" | "!" | "[" | "]" | "{" | "}" => return None,
                        _ => (),
                    }
                }
            } else {
                let slot = match key {
                    "name" => &mut name,
                    "kind" => &mut kind,
                    "modifiers" => &mut modifiers,
                    _ => return None,
                };
                if tokens.next()? != "=" {
                    return None;
                }
                let text = tokens.next()?.strip_prefix('"')?.strip_suffix('"')?;
                if slot.replace(text).is_some()
                    || text.is_empty()
                    || !text
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_+.-".contains(&b))
                {
                    return None;
                }
            }
            match tokens.next()? {
                "," => (),
                ")" => break,
                _ => return None,
            }
        }
        Some(
            !name?.starts_with('-')
                && matches!(
                    (kind, modifiers),
                    (None | Some("dylib"), None) | (Some("static"), Some("-bundle"))
                ),
        )
    }
    parse(input) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_literal_linkage_deferred_to_the_native_linker_is_admitted() {
        for input in [
            r#"(name="c")"#,
            r#"(name="rt",kind="static",modifiers="-bundle",cfg(not(target_feature="crt-static")))"#,
        ] {
            assert!(unbundled_link(input));
        }
        for input in [
            r#"(name="x",kind="static")"#,
            r#"(name="x",kind="static",modifiers="+bundle")"#,
            r#"(name=compute!())"#,
            r#"(name="/some/file")"#,
            r#"(name="c",kind="dylib",kind="static")"#,
        ] {
            assert!(!unbundled_link(input));
        }
    }
}
