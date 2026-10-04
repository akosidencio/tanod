#![no_main]
//! Query-string canonicalisation.
//!
//! Two failure modes, in opposite directions. Merging parameters that matter
//! serves one page's content at another page's URL. Splitting parameters that
//! do not matter mints a cache entry — and therefore a render — per request,
//! which is the load the governor exists to prevent.

use arbitrary::Arbitrary;
use tanod::config::schema::{QueryMode, QueryPolicy};
use tanod::fuzzing::canonical_query;
use libfuzzer_sys::fuzz_target;

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

    // The filter must do exactly what it says, for every key it kept.
    for pair in out.split('&').filter(|p| !p.is_empty()) {
        let key = pair.split('=').next().unwrap_or("");
        match input.mode {
            Mode::None => {}
            Mode::Include => assert!(
                input.keys.iter().any(|k| k == key),
                "`include` kept unlisted key `{key}`",
            ),
            Mode::Exclude => assert!(
                !input.keys.iter().any(|k| k == key),
                "`exclude` kept excluded key `{key}`",
            ),
        }
    }
});
