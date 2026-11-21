//! rustc dep-info describes Rust inputs. Assembly and native link attributes
//! can open additional files behind rustc's back. Inspect the successful
//! compilation's input set once, before admitting any of its artifacts.
use anyhow::{Result, ensure};
use memchr::memmem;
use rustc_lexer::{TokenKind, first_token, tokenize};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::Command;

mod assembly;
mod nightly;
mod review;
mod text;
pub use nightly::{check_root, verify_conditions};
pub(crate) use review::{Candidate, Origin, Review};
pub(crate) use text::data_only;

#[derive(Clone, Copy)]
pub(super) struct Words<'a>(pub(super) &'a str);

impl<'a> Iterator for Words<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<Self::Item> {
        while !self.0.is_empty() {
            let t = first_token(self.0);
            let (word, rest) = self.0.split_at(t.len);
            self.0 = rest;
            if !matches!(
                t.kind,
                TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { .. }
            ) {
                return Some(word);
            }
        }
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable = 1,
    Nightly = 2,
    Unsupported = 3,
}

impl Channel {
    pub fn release(version: &str) -> Self {
        let number = version.strip_suffix("-nightly").unwrap_or(version);
        let mut parts = number.split('.');
        if !(0..3).all(|_| {
            parts
                .next()
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        }) || parts.next().is_some()
        {
            Self::Unsupported
        } else if number == version {
            Self::Stable
        } else {
            Self::Nightly
        }
    }
}

pub fn compiler_channel(compiler: &OsStr) -> Result<Channel> {
    let version = Command::new(compiler).arg("-V").output()?;
    let output = std::str::from_utf8(&version.stdout)?;
    ensure!(version.status.success(), "could not identify rustc version");
    Ok(output
        .strip_prefix("rustc ")
        .and_then(|v| v.split_whitespace().next())
        .map_or(Channel::Unsupported, Channel::release))
}

pub fn verify_bytes(
    bytes: &[u8],
    path: &Path,
    deferred_links: bool,
    observed: bool,
    reviewed_assembly: bool,
) -> Result<bool> {
    if [b"asm".as_slice(), b"link"]
        .iter()
        .any(|p| memmem::find(bytes, p).is_some())
        && let Ok(src) = std::str::from_utf8(bytes)
    {
        return inspect_source(src, deferred_links, observed, reviewed_assembly)
            .map_err(|reason| anyhow::anyhow!("{reason} in {}", path.display()));
    }
    Ok(false)
}

// A dep-info file can be a Rust module or data for include_bytes!. A
// successful stable compiler cannot have parsed invalid UTF-8 as source.
// For oversized UTF-8 inputs, prove the absence of the two sensitive token
// substrings. Keep three bytes across reads for both split UTF-8 and "link".
pub fn large_input(mut file: impl Read, path: &Path) -> Result<blake3::Hash> {
    let mut hash = blake3::Hasher::new();
    let mut window = vec![0; 64 * 1024 + 3];
    let mut prefix = 0;
    let mut incomplete = 0;
    let mut sensitive = false;
    loop {
        let n = match file.read(&mut window[prefix..]) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            read => read?,
        };
        if n == 0 {
            ensure!(
                incomplete != 0 || !sensitive,
                "large UTF-8 input needs a full Rust admission scan: {}",
                path.display()
            );
            break;
        }
        let end = prefix + n;
        hash.update(&window[prefix..end]);
        let utf8 = &window[prefix - incomplete..end];
        incomplete = match std::str::from_utf8(utf8) {
            Ok(_) => 0,
            Err(e) if e.error_len().is_none() => utf8.len() - e.valid_up_to(),
            Err(_) => {
                hash.update_reader(file)?;
                break;
            }
        };
        if !sensitive {
            sensitive = [b"asm".as_slice(), b"link"]
                .iter()
                .any(|word| memmem::find(&window[..end], word).is_some());
        }
        prefix = end.min(3);
        window.copy_within(end - prefix..end, 0);
    }
    Ok(hash.finalize())
}

fn inspect_source(
    source: &str,
    deferred_links: bool,
    observed: bool,
    reviewed_assembly: bool,
) -> std::result::Result<bool, &'static str> {
    let mut extended = false;
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
                // libc declares modules literally named asm. A module name or
                // a namespace prefix in ordinary Rust cannot call or alias an
                // assembly macro. Inside a macro token tree it could do either.
                let mut next = Words(input);
                let first = next.next();
                let prefix = first == Some(":")
                    && next.next() == Some(":")
                    && next.next().is_some_and(|w| {
                        matches!(first_token(w).kind, Ident | RawIdent)
                            && !matches!(w, "self" | "super" | "crate" | "Self")
                    });
                let module = !sensitive.last().is_some_and(|&(group, _)| group)
                    && (prev == "mod" && matches!(first, Some(";" | "{")) || prefix);
                if !module {
                    let bare_import = prev == ":" && matches!(first, Some(";" | "," | "}"));
                    if reviewed_assembly
                        || word == "asm"
                            && !sensitive.last().is_some_and(|&(group, _)| group)
                            && (bare_import || assembly::closed_template(input))
                    {
                        extended = true;
                    } else {
                        return Err("assembly input");
                    }
                }
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
                    return Err("native link attribute");
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
    Ok(extended)
}

#[cfg(test)]
fn opaque_input(source: &str, deferred_links: bool, observed: bool) -> Option<&'static str> {
    inspect_source(source, deferred_links, observed, false).err()
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
    fn a_reviewed_assembly_site_still_checks_native_link_attributes() {
        let src = r#"cfg_if! { if #[cfg(unix)] { pub use asm::socket::*; } }
                     #[link(name = "archive", kind = "static")] extern "C" {}"#;
        assert_eq!(
            inspect_source(src, false, false, true),
            Err("native link attribute")
        );
    }
    #[test]
    fn ordinary_asm_modules_do_not_hide_macros_or_aliases() {
        assert!(opaque_input("pub(crate) mod asm; use asm::types::*;", false, false).is_none());
        for source in [
            "use core::arch::asm as op;",
            "pub use core::arch::asm::{self as op};",
            "core::arch::asm!(include_str!(\"code.s\"))",
            "make!(mod asm;)",
            "macro_rules! m { () => { mod asm; } }",
        ] {
            assert!(opaque_input(source, false, false).is_some(), "{source}");
        }
    }
    #[test]
    fn streaming_admission_handles_split_tokens_and_utf8() -> Result<()> {
        let path = Path::new("asset");
        let data = |a: &'static [u8], b: &'static [u8]| a.chain(b);
        assert!(large_input(data(b"global_a", b"sm!"), path).is_err());
        assert!(large_input(data(b"li", b"nk"), path).is_err());
        assert!(large_input(data(b"asm\xe2", b"\x82\xac"), path).is_err());
        assert_eq!(
            large_input(data(b"asm\xe2", b"\xff"), path)?,
            blake3::hash(b"asm\xe2\xff")
        );
        assert_eq!(
            large_input(data(b"generated ", b"123"), path)?,
            blake3::hash(b"generated 123")
        );
        Ok(())
    }
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
