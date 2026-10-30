#![no_main]
//! Query-string canonicalisation.
//!
//! Two failure modes, in opposite directions. Merging parameters that matter
//! serves one page's content at another page's URL. Splitting parameters that
//! do not matter mints a cache entry — and therefore a render — per request,
//! which is the load the governor exists to prevent.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tanod::config::schema::{QueryMode, QueryPolicy};
use tanod::fuzzing::canonical_query;

#[derive(Arbitrary, Debug)]
enum Mode {
    None,
    Include,
    Exclude,
}

#[derive(Arbitrary, Debug)]
struct Input {
    raw: String,
    mode: Mode,
    keys: Vec<String>,
}

fuzz_target!(|input: Input| {
    let policy = match input.mode {
        Mode::None => None,
        Mode::Include => Some(QueryPolicy {
            mode: QueryMode::Include,
            keys: input.keys.clone(),
        }),
        Mode::Exclude => Some(QueryPolicy {
            mode: QueryMode::Exclude,
            keys: input.keys.clone(),
        }),
    };

    let out = canonical_query(&input.raw, policy.as_ref());

    // Canonicalising an already-canonical string must be a no-op, or the entry
    // a request lands in depends on how many times the value was normalised.
    assert_eq!(canonical_query(&out, policy.as_ref()), out);

    // The filter must do exactly what it says, for every key it kept. A policy
    // names parameters as the origin reads them, so `%71` is `q` and `+` is a
    // space: comparing the raw spelling would let `?%71=x` past `include: [q]`.
    for pair in out.split('&').filter(|p| !p.is_empty()) {
        let key = pair.split('=').next().unwrap_or("");
        let name = origin_name(key);
        match input.mode {
            Mode::None => {}
            Mode::Include => assert!(
                input.keys.iter().any(|k| k.as_bytes() == name),
                "`include` kept unlisted key `{key}`",
            ),
            Mode::Exclude => assert!(
                !input.keys.iter().any(|k| k.as_bytes() == name),
                "`exclude` kept excluded key `{key}`",
            ),
        }
    }
});

/// A parameter name the way `URLSearchParams` decodes it: `+` is a space,
/// `%XX` is a byte, and a malformed escape stays literal. Written here rather
/// than borrowed from the crate, so the oracle does not share the code under
/// test.
fn origin_name(raw: &str) -> Vec<u8> {
    let bytes = raw.as_bytes();
    let hex = |at: usize| {
        bytes
            .get(at)
            .and_then(|b| char::from(*b).to_digit(16))
            .map(|d| d as u8)
    };
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' => match (hex(at + 1), hex(at + 2)) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    at += 3;
                }
                _ => {
                    out.push(b'%');
                    at += 1;
                }
            },
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}
