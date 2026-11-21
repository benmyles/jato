//! rustc collects language features from crate-root attributes before expansion.
//! Only cfg predicates, never user items or macros, enter the isolated cfg probe.
use super::Words;
use crate::invocation::Invocation;
use anyhow::{Context, Result, ensure};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn ident(word: &str) -> &str {
    word.strip_prefix("r#").unwrap_or(word)
}

// These APIs affect documentation or the procedural macro host. Compiling
// their declarations does not execute a macro. Macro consumers still bypass.
fn known_features(mut words: Words<'_>) -> bool {
    if words.next() != Some("(") {
        return false;
    }
    loop {
        match words.next().map(ident) {
            Some(")") => return true,
            Some("doc_cfg" | "doc_auto_cfg" | "proc_macro_span" | "proc_macro_def_site") => (),
            _ => return false,
        }
        match words.next() {
            Some(")") => return true,
            Some(",") => (),
            _ => return false,
        }
    }
}

// Accept a bounded tree of cfg names, literal values and parentheses. Reject
// raw expressions, user macro calls and attribute syntax. Consume just the
// feature identifier so every subsequent attribute still gets checked.
fn conditional(mut words: Words<'_>) -> Option<(String, Words<'_>)> {
    if words.next()? != "(" {
        return None;
    }
    let mut predicate = String::new();
    let mut depth = 0usize;
    let mut previous = "";
    loop {
        let word = words.next()?;
        match word {
            "," if depth == 0 => break,
            "(" if matches!(
                ident(previous),
                "all" | "any" | "not" | "version" | "target"
            ) =>
            {
                depth += 1;
            }
            "(" => return None,
            ")" => depth = depth.checked_sub(1)?,
            "," | "=" => (),
            _ if ident(word)
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_') => {}
            _ if word
                .strip_prefix('"')?
                .strip_suffix('"')?
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-.+".contains(&c)) => {}
            _ => return None,
        }
        predicate.push_str(word);
        predicate.push(' ');
        previous = word;
        if predicate.len() > 1024 || depth > 32 {
            return None;
        }
    }
    (ident(words.next()?) == "feature" && words.clone().next() == Some("("))
        .then_some((predicate, words))
}

pub fn check_root(bytes: &[u8], guards: &mut Vec<String>) -> Result<()> {
    if memchr::memmem::find(bytes, b"feature").is_none() {
        return Ok(());
    }
    let source = std::str::from_utf8(bytes)?.trim_start_matches('\u{feff}');
    let source = if let Some(rest) = source.strip_prefix("#!")
        && Words(rest).next() != Some("[")
    {
        source.split_once('\n').map_or("", |(_, body)| body)
    } else {
        source
    };
    let mut words = Words(source);
    let mut previous = "";
    while let Some(word) = words.next() {
        let name = ident(word);
        if name == "feature" && words.clone().next() == Some("(") {
            ensure!(
                known_features(words),
                "nightly_feature: unverified crate-root language feature"
            );
        }
        if previous == "["
            && name == "cfg_attr"
            && let Some((predicate, after)) = conditional(words)
        {
            if !known_features(after) && !guards.contains(&predicate) {
                ensure!(guards.len() < 32, "too many nightly feature predicates");
                guards.push(predicate);
            }
            words = after;
        }
        previous = word;
    }
    Ok(())
}

pub fn verify_conditions(
    compiler: &OsStr,
    inv: &Invocation,
    dir: &Path,
    guards: &[String],
) -> Result<()> {
    if guards.is_empty() {
        return Ok(());
    }
    let output = crate::staging::file(dir)?;
    let mut emit = OsString::from("--emit=dep-info=");
    emit.push(output.path());
    let mut child = Command::new(compiler)
        .args(&inv.probe_args[..inv.source_arg])
        .arg("-")
        .args(&inv.probe_args[inv.source_arg + 1..])
        .arg(emit)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().context("opening feature probe input")?;
    // A duplicate module causes a name-resolution error before dep-info is
    // emitted. The original compiler evaluates the original unit's cfg flags.
    let written = (|| -> std::io::Result<()> {
        writeln!(stdin, "#![no_std]")?;
        for (i, predicate) in guards.iter().enumerate() {
            writeln!(
                stdin,
                "mod jato_{i} {{}}\n#[cfg({predicate})] mod jato_{i} {{}}"
            )?;
        }
        Ok(())
    })();
    drop(stdin);
    let status = child.wait()?;
    written?;
    ensure!(
        status.success(),
        "nightly_feature: compiler could not rule out an active language feature"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_gates_only_export_pure_predicates() {
        let mut guards = Vec::new();
        check_root(br#"#![cfg_attr(any(r#docsrs,feature = "nightly"), r#feature /* comment */ (rustc_attrs))]
                     #![cfg_attr(docsrs, feature(doc_cfg))]
                     #![feature(proc_macro_span,proc_macro_def_site)]"#, &mut guards).unwrap();
        assert_eq!(guards, ["any ( r#docsrs , feature = \"nightly\" ) "]);
        for source in [
            "#!/bin/sh /*\n#![feature(rustc_attrs)]",
            "#![cfg_attr(custom!(), feature(x))]",
            "#![cfg_attr(custom(key), feature(x))]",
            "#![cfg_attr(outer, cfg_attr(inner, feature(x)))]",
            "#![feature(doc_cfg, rustc_attrs)]",
            "#![cfg_attr(docsrs,feature(x))]\n#![feature(y)]",
        ] {
            assert!(check_root(source.as_bytes(), &mut Vec::new()).is_err());
        }
    }
}
