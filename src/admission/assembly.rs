//! Recognize closed inline templates in ordinary Rust syntax. No symbol, const,
//! macro-supplied string, assembler directive, or second statement is accepted.
use super::Words;

fn literal_instruction(word: &str) -> bool {
    let Some(text) = word.strip_prefix('"').and_then(|w| w.strip_suffix('"')) else {
        return false;
    };
    let text = text.trim_matches([' ', '\t']);
    if text.is_empty() {
        return true;
    }
    let comment = text
        .strip_prefix("/*")
        .and_then(|s| s.strip_suffix("*/"))
        .or_else(|| text.strip_prefix("//"))
        .or_else(|| text.strip_prefix('#'));
    if let Some(comment) = comment {
        let comment = comment.trim_matches([' ', '\t']);
        return comment.is_empty()
            || comment
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .is_some_and(|register| {
                    register
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_:".contains(&b))
                });
    }
    let mnemonic = text.split_ascii_whitespace().next().unwrap_or("");
    let instruction = matches!(
        mnemonic,
        "nop"
            | "mov"
            | "movq"
            | "xor"
            | "add"
            | "sub"
            | "adc"
            | "sbb"
            | "mul"
            | "div"
            | "sqrtsd"
            | "sqrtss"
            | "lfence"
            | "sfence"
            | "mfence"
            | "vfmadd213sd"
            | "vfmadd213ss"
            | "vfmaddsd"
            | "vfmaddss"
    ) || (mnemonic == "lock"
        && text.split_ascii_whitespace().nth(1) == Some("not"));
    instruction
        && text
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b" \t_{}[],:+*-%".contains(&c))
}

pub(super) fn closed_template(input: &str) -> bool {
    let mut words = Words(input).peekable();
    if words.next() != Some("!") || words.next() != Some("(") {
        return false;
    }
    let mut saw_template = false;
    let mut operands = false;
    loop {
        let Some(mut head) = words.next() else {
            return false;
        };
        if head == ")" {
            return saw_template;
        }
        if !operands && literal_instruction(head) {
            saw_template = true;
        } else {
            if !saw_template {
                return false;
            }
            operands = true;
            if words.peek() == Some(&"=") {
                words.next();
                let Some(kind) = words.next() else {
                    return false;
                };
                head = kind;
            }
            if !matches!(
                head,
                "in" | "out" | "lateout" | "inout" | "inlateout" | "options"
            ) || words.next() != Some("(")
            {
                return false;
            }
            let mut depth = 1;
            while let Some(&word) = words.peek() {
                match word {
                    "," | ")" if depth == 0 => break,
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => {
                        if depth == 0 {
                            return false;
                        }
                        depth -= 1;
                    }
                    _ => (),
                }
                words.next();
            }
            if depth != 0 {
                return false;
            }
        }
        match words.next() {
            Some(",") => (),
            Some(")") => return saw_template,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_closed_literal_instructions_and_registers_are_eligible() {
        for input in [
            r##"!("# {}", in(reg) ptr, options(nostack, preserves_flags))"##,
            r#"! ("/*{0}*/", inout(reg) p)"#,
            r#"! ("vfmadd213sd {x}, {y}, {z}", x = inout(xmm_reg) x, y = in(xmm_reg) y, z = in(xmm_reg) z)"#,
            r#"! ("lock not dword ptr [{0:e}]", in(reg) a.get(), options(nostack))"#,
        ] {
            assert!(closed_template(input), "{input}");
        }
        for input in [
            r#"!(".include \"asset\"")"#,
            r#"! (include_str!("asset"))"#,
            r#"! ("nop\n.include x")"#,
            r#"! ("nop ; mov eax, ebx")"#,
            r#"! ("nop", sym target)"#,
            r#"! ("/*{x}*/", x = const 0)"#,
            r#"! ("/*comment*/ .incbin file /*x*/")"#,
            r#"! { "nop" }"#,
        ] {
            assert!(!closed_template(input), "{input}");
        }
    }
}
