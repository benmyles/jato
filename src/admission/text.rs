//! A tracked file may be Rust source under any extension, or bytes included
//! as documentation. A fatal lexer error from the same compiler distinguishes
//! a common ambiguity without dropping the file from the content key.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;
use std::process::{Command, Stdio};

pub(crate) fn data_only(
    compiler: &OsStr,
    args: &[OsString],
    path: &Path,
    file: &mut File,
    bytes: &[u8],
) -> bool {
    if path.extension() == Some(OsStr::new("rs")) || bytes.len() > 1 << 20 || !bytes.contains(&b'`')
    {
        return false;
    }
    let mut cmd = Command::new(compiler);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--edition" {
            let Some(edition) = args.next() else {
                return false;
            };
            cmd.arg(arg).arg(edition);
        } else if arg.as_encoded_bytes().starts_with(b"--edition=") {
            cmd.arg(arg);
        }
    }
    if file.rewind().is_err() {
        return false;
    }
    let Ok(input) = file.try_clone() else {
        return false;
    };
    let Ok(mut child) = cmd
        .args([
            "--crate-name",
            "jato_text",
            "--crate-type=lib",
            "--print=crate-name",
            "--error-format=json",
            "-",
        ])
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    // Asking for the crate name parses the input without running user items or
    // macros. Ignore grammar errors: a valid include! can be a single expression.
    let proof = BufReader::new(child.stderr.take().unwrap().take(64 << 10))
        .split(b'\n')
        .map_while(Result::ok)
        .any(|line| {
            let Ok(m) = serde_json::from_slice::<serde_json::Value>(&line) else {
                return false;
            };
            m["level"] == "error"
                && m["message"] == "unknown start of token: `"
                && m["spans"].as_array().is_some_and(|spans| {
                    spans.iter().any(|s| {
                        let Some(i) = s["byte_start"]
                            .as_u64()
                            .and_then(|i| usize::try_from(i).ok())
                        else {
                            return false;
                        };
                        s["is_primary"] == true
                            && s["file_name"] == "<anon>"
                            && s["byte_end"].as_u64() == Some(i as u64 + 1)
                            && bytes.get(i) == Some(&b'`')
                    })
                })
        });
    let _ = child.kill();
    let reaped = child.wait().is_ok();
    proof && reaped
}
