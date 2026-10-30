#![no_main]
//! Differentially check indexed route selection against actual matcher order.
use http::Method;
use libfuzzer_sys::fuzz_target;
use tanod::config::schema::{Config, Matcher, Route};
use tanod::policy::PolicySnapshot;

fuzz_target!(|data: &str| {
    let mut lines = data.lines();
    let host = lines.next().unwrap_or("shop.example");
    let path = lines.next().unwrap_or("/");
    let mut config: Config =
        serde_saphyr::from_str("version: 1\norigin:\n  upstreams: ['127.0.0.1:9']\n").unwrap();
    for (index, pattern) in lines.take(32).enumerate() {
        let mut route: Route = serde_saphyr::from_str("id: seed\nmatch: '/**'\n").unwrap();
        route.id = index.to_string();
        route.matcher = Matcher::Path(pattern.chars().take(128).collect());
        config.routes.push(route);
    }
    let Ok(snapshot) = PolicySnapshot::build(config, 1) else {
        return;
    };
    for method in [Method::GET, Method::HEAD, Method::POST] {
        let actual = snapshot.resolve(host, path, &method).map(|r| &r.id);
        let reference = snapshot
            .routes
            .iter()
            .find(|r| r.matcher.matches(host, path, &method))
            .map(|r| &r.id);
        assert_eq!(actual, reference);
    }
});
