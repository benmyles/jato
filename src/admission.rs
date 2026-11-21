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

pub fn verify_bytes(bytes: &[u8], path: &Path, deferred_links: bool, observed: bool) -> Result<()> {
    if [b"asm".as_slice(), b"link"]
        .iter()
        .any(|p| memmem::find(bytes, p).is_some())
        && let Ok(src) = std::str::from_utf8(bytes)
        && let Some(reason) = opaque_input(src, deferred_links, observed)
    {
        anyhow::bail!("{reason} in {}", path.display());
    }
    Ok(())
}

fn opaque_input(source: &str, deferred_links: bool, observed: bool) -> Option<&'static str> {
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
                if word == "link"
                    && prev != "."
                    && sensitive.last().is_some_and(|&(group, _)| group) =>
            {
                let in_macro = sensitive.last().is_some_and(|&(_, macros)| macros);
                let allowed = match literal_link(input) {
                    Some(LinkKind::Deferred) => deferred_links || observed && !in_macro,
                    Some(LinkKind::Bundled) => observed && !in_macro,
                    None => false,
                };
                if !matches!(prev, "[" | ",") || !allowed {
                    return Some("native link attribute");
                }
            }
            Ident if word == "macro_rules" => macro_definition = true,
            OpenBrace | OpenBracket | OpenParen => {
                let (outer, in_macro) = sensitive.last().copied().unwrap_or((false, false));
                let macros = macro_definition || prev == "!" || in_macro;
                sensitive.push((macros || outer || prev == "#", macros));
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    Deferred,
    Bundled,
}

// Bundled archives enter an rlib during compilation. An observed invocation
// can certify that read, but cannot certify a future expansion of an exported macro.
fn literal_link(input: &str) -> Option<LinkKind> {
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
    if name?.starts_with('-') {
        return None;
    }
    match (kind, modifiers) {
        (None | Some("dylib"), None) | (Some("static"), Some("-bundle")) => {
            Some(LinkKind::Deferred)
        }
        (Some("static"), None | Some("+bundle")) => Some(LinkKind::Bundled),
        _ => None,
    }
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
            assert!(literal_link(input) == Some(LinkKind::Deferred));
        }
        for input in [
            r#"(name="x",kind="static")"#,
            r#"(name="x",kind="static",modifiers="+bundle")"#,
            r#"(name=compute!())"#,
            r#"(name="/some/file")"#,
            r#"(name="c",kind="dylib",kind="static")"#,
        ] {
            assert!(literal_link(input) != Some(LinkKind::Deferred));
        }
        let bundled = r#"#[link(name="x", kind="static")] extern "C" {}"#;
        assert!(opaque_input(bundled, true, false).is_some());
        assert!(opaque_input(bundled, true, true).is_none());
        assert!(
            opaque_input(
                &format!("macro_rules! export {{ () => {{ {bundled} }} }}"),
                true,
                true
            )
            .is_some()
        );
    }
}
