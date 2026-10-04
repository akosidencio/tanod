#![no_main]
//! Config parsing and validation.
//!
//! Every accepted config is a claim about what is being protected. The rule
//! this target defends is that parsing and validation are total: a malformed
//! file must produce an error naming what was wrong, never a panic and never
//! an accepted config that validation was supposed to refuse.

use tanod::config::{Config, validation};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let Ok(config) = serde_saphyr::from_str::<Config>(data) else {
        return;
    };

    // Validation runs on everything the parser accepts, and must terminate
    // with a verdict rather than a panic.
    let verdict = validation::validate(&config);

    // Validation is a pure function of the config; running it twice must not
    // change its mind.
    assert_eq!(verdict.is_ok(), validation::validate(&config).is_ok());

    if verdict.is_ok() {
        // An accepted config may never carry the combination the fence exists
        // to prevent: an override that stores responses the origin refused,
        // with no ceiling on how long they are kept.
        for route in &config.routes {
            if let Some(cache) = &route.cache
                && cache.override_origin
            {
                assert!(
                    cache.ttl.as_ref().and_then(|t| t.max).is_some(),
                    "route `{}` overrides the origin with no ttl.max",
                    route.id,
                );
            }
        }
    }
});
