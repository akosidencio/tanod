//! `${VAR}` references in the config file, filled in from the environment.
//!
//! This exists so a secret never has to be written into a file or baked into
//! an image: `token: "${TANOD_PURGE_TOKEN}"` reads it from the process
//! environment at load. Without it, every deployment grew its own entrypoint
//! script that templated the secret into the config with `sed`.
//!
//! Expansion is textual and happens before parsing, which is what makes it
//! work for every key without a per-field `_env` variant. The price of
//! textual is that a value could change the document's *structure*, so values
//! are restricted to characters that cannot: no whitespace, quotes,
//! backslashes, `#` or control characters. That still admits every token,
//! URL, hostname and base64 credential this config holds, and anything else is
//! refused with the variable's name rather than parsed into something nobody
//! wrote.
//!
//! - `${NAME}` — required. Unset is an error, never an empty string.
//! - `${NAME:-default}` — `default` when `NAME` is unset or empty.
//! - `$${` — a literal `${`.
//! - Inside a YAML comment nothing is expanded, so a commented-out line can
//!   never fail a load.

/// Expand every reference in `text`, reading variables through `lookup`.
pub fn expand(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split_inclusive('\n').enumerate() {
        expand_line(line, index + 1, &lookup, &mut out)?;
    }
    Ok(out)
}

fn expand_line(
    line: &str,
    number: usize,
    lookup: &impl Fn(&str) -> Option<String>,
    out: &mut String,
) -> Result<(), String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let (mut single, mut double) = (false, false);
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            // An escaped character inside a double-quoted scalar: copy the
            // backslash and the whole character after it, which may be more
            // than one byte.
            b'\\' if double && i + 1 < bytes.len() => {
                let width = line[i + 1..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&line[i..i + 1 + width]);
                i += 1 + width;
                continue;
            }
            // A comment starts at `#` outside quotes, at the start of the line
            // or after whitespace. The rest of the line is copied verbatim.
            b'#' if !single
                && !double
                && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') =>
            {
                out.push_str(&line[i..]);
                return Ok(());
            }
            b'$' if line[i..].starts_with("$${") => {
                out.push_str("${");
                i += 3;
                continue;
            }
            b'$' if line[i..].starts_with("${") => {
                let close = line[i + 2..]
                    .find('}')
                    .ok_or_else(|| format!("line {number}: unclosed `${{`"))?;
                let inner = &line[i + 2..i + 2 + close];
                out.push_str(&resolve(inner, number, lookup)?);
                i += close + 3;
                continue;
            }
            _ => {}
        }
        // Copy one whole character, not one byte, so UTF-8 survives.
        let width = line[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[i..i + width]);
        i += width;
    }
    Ok(())
}

fn resolve(
    inner: &str,
    number: usize,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let (name, default) = match inner.split_once(":-") {
        Some((name, default)) => (name, Some(default)),
        None => (inner, None),
    };
    let valid_name = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_name {
        return Err(format!(
            "line {number}: `${{{inner}}}` is not a variable reference; names are letters, digits and `_`"
        ));
    }
    let value = match (lookup(name).filter(|v| !v.is_empty()), default) {
        (Some(value), _) => value,
        (None, Some(default)) => default.to_string(),
        (None, None) => {
            return Err(format!(
                "line {number}: environment variable {name} is not set (write `${{{name}:-default}}` to make it optional)"
            ));
        }
    };
    if let Some(bad) = value
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\' | '#'))
    {
        return Err(format!(
            "line {number}: the value of {name} contains {bad:?}, which could change how the config \
             parses; whitespace, quotes, backslashes, `#` and control characters are not allowed"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn a_reference_is_filled_from_the_environment() {
        let out = expand(
            "token: \"${TOKEN}\"\nurl: http://${HOST}:4318/v1\n",
            env(&[("TOKEN", "abc_123-XYZ"), ("HOST", "collector")]),
        )
        .unwrap();
        assert_eq!(
            out,
            "token: \"abc_123-XYZ\"\nurl: http://collector:4318/v1\n"
        );
    }

    #[test]
    fn an_unset_variable_is_an_error_naming_it_and_its_line() {
        let err = expand("a: 1\ntoken: ${MISSING}\n", env(&[])).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("MISSING is not set"), "{err}");
    }

    #[test]
    fn a_default_covers_an_unset_or_empty_variable() {
        let out = expand(
            "a: ${UNSET:-fallback}\nb: ${EMPTY:-other}\nc: ${SET:-unused}\n",
            env(&[("EMPTY", ""), ("SET", "real")]),
        )
        .unwrap();
        assert_eq!(out, "a: fallback\nb: other\nc: real\n");
    }

    #[test]
    fn a_value_cannot_change_the_document_structure() {
        for value in [
            "x\ninjected: true",
            "a b",
            "a\"b",
            "a'b",
            "a\\b",
            "a#b",
            "\t",
        ] {
            let lookup = move |_: &str| Some(value.to_string());
            assert!(expand("token: \"${T}\"\n", lookup).is_err(), "{value:?}");
        }
    }

    #[test]
    fn comments_are_left_alone() {
        let out = expand(
            "# token: ${NOT_SET}\nurl: \"a#b\" # also ${NOT_SET}\nkey: v#notacomment\n",
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            out,
            "# token: ${NOT_SET}\nurl: \"a#b\" # also ${NOT_SET}\nkey: v#notacomment\n"
        );
    }

    #[test]
    fn a_dollar_dollar_escape_is_a_literal() {
        assert_eq!(expand("a: $${HOME}\n", env(&[])).unwrap(), "a: ${HOME}\n");
        assert_eq!(
            expand("a: $5 and $x\n", env(&[])).unwrap(),
            "a: $5 and $x\n"
        );
    }

    #[test]
    fn malformed_references_are_refused() {
        assert!(
            expand("a: ${OPEN\n", env(&[]))
                .unwrap_err()
                .contains("unclosed")
        );
        assert!(expand("a: ${1BAD}\n", env(&[])).is_err());
        assert!(expand("a: ${}\n", env(&[])).is_err());
    }

    #[test]
    fn a_backslash_before_a_multibyte_character_does_not_panic() {
        // Not a valid YAML escape, but load must reject it as a parse error
        // later rather than panic here on a split UTF-8 character.
        let text = "title: \"caf\\é ${A:-x}\"\nmore: \"\\🙂\"\n";
        assert_eq!(
            expand(text, env(&[])).unwrap(),
            "title: \"caf\\é x\"\nmore: \"\\🙂\"\n"
        );
    }

    #[test]
    fn text_without_references_is_unchanged() {
        let text = "version: 1\nname: \"Sandali lang po — ñ\"\n";
        assert_eq!(expand(text, env(&[])).unwrap(), text);
    }

    proptest::proptest! {
        /// Whatever the file holds, expansion returns or refuses; it never
        /// panics (the slicing above is byte-indexed over UTF-8).
        #[test]
        fn expansion_never_panics(text in "\\PC{0,200}") {
            let _ = expand(&text, |name| (name.len() % 2 == 0).then(|| "v".to_string()));
        }

        /// Text with no `$` at all is returned byte for byte.
        #[test]
        fn text_without_a_dollar_is_unchanged(text in "[^$]{0,200}") {
            proptest::prop_assert_eq!(expand(&text, |_| None).unwrap(), text);
        }

        /// A reference resolves to the value, and only the reference changes.
        #[test]
        fn a_reference_is_replaced_and_nothing_else(
            before in "[a-z :]{0,20}",
            after in "[a-z :]{0,20}",
            value in "[A-Za-z0-9_./-]{1,30}",
        ) {
            let text = format!("{before}${{NAME}}{after}");
            let out = expand(&text, |_| Some(value.clone())).unwrap();
            proptest::prop_assert_eq!(out, format!("{before}{value}{after}"));
        }
    }
}
