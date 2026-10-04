#![no_main]
//! Origin-declared `Vary` against the headers the cache key actually carries.
//!
//! Returning `None` means "this response may be stored". If it ever does so
//! for a header the key does not carry, the entry will be served to requests
//! that should have received a different body.

use arbitrary::Arbitrary;
use tanod::fuzzing::unsupported_vary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    vary: String,
    key_headers: Vec<String>,
}

fuzz_target!(|input: Input| {
    let verdict = unsupported_vary(&input.vary, &input.key_headers);

    if verdict.is_none() {
        // Storage was permitted, so every token the origin named must be one
        // the key can honour.
        for token in input.vary.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if token.is_empty() {
                continue;
            }
            assert_ne!(token, "*", "`Vary: *` was accepted");
            assert!(
                token == "accept-encoding"
                    || input
                        .key_headers
                        .iter()
                        .any(|k| k.eq_ignore_ascii_case(&token)),
                "accepted `{token}` which the key does not carry",
            );
        }
    }

    // Adding a token can only ever make the answer stricter, never looser.
    let widened = format!("{}, x-not-in-any-key", input.vary);
    assert!(unsupported_vary(&widened, &input.key_headers).is_some());
});
