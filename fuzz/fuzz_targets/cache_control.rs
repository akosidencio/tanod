#![no_main]
//! `Cache-Control` parsing.
//!
//! The header arrives from the origin, which for anything proxying third-party
//! content means it is attacker-influenced. Parsing runs on the response path,
//! where a panic takes the proxy down rather than failing one request.

use tanod::cache::policy::CacheControl;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let parsed = CacheControl::parse(data);

    // Parsing must be total and stable.
    assert_eq!(parsed, CacheControl::parse(data));
    let _ = parsed.shared_ttl();

    // A shared cache honours `s-maxage` ahead of `max-age`; that difference is
    // what separates it from a browser cache.
    match (parsed.s_maxage, parsed.max_age) {
        (Some(shared), _) => assert_eq!(
            parsed.shared_ttl(),
            Some(std::time::Duration::from_secs(shared))
        ),
        (None, Some(private_age)) => assert_eq!(
            parsed.shared_ttl(),
            Some(std::time::Duration::from_secs(private_age))
        ),
        (None, None) => assert_eq!(parsed.shared_ttl(), None),
    }

    // Prepending a refusal must never be lost. `no-store` is the origin's
    // clearest possible instruction and only a fenced route override outranks
    // it — never a parsing accident.
    assert!(CacheControl::parse(&format!("no-store,{data}")).no_store);
    assert!(CacheControl::parse(&format!("{data},no-store")).no_store);
});
