//! Regression test for `web.redirectToHttps.status`.
//!
//! The admin console offers `[301, 308, 302, 303, 307]`
//! (`admin-console/src/features/sites/site-settings-fixtures.ts:228`). The edge
//! must emit every one of those verbatim and fall back to `301` only for values
//! outside that set. `303` was previously dropped and coerced to `301`.
//!
//! This exercises the production parse + compile path:
//! `ServerConfig`(serde) → `CompiledServerFeaturePlan::compile` →
//! `redirect_to_https_target`, which is the runtime's preferred path
//! (`proxy.rs` `resolve_https_redirect`). The runtime fallback in
//! `EdgeProxy::redirect_to_https_status` shares the same
//! `compiled::redirect_to_https_status` filter (covered by a unit test there).

use zelvik_edge::compiled::CompiledServerFeaturePlan;
use zelvik_edge::config_models::ServerConfig;

fn effective_status(status: i32) -> u16 {
    let server: ServerConfig = serde_json::from_value(serde_json::json!({
        "id": 9001,
        "isOn": true,
        "serverNames": [{"name": "a.example"}],
        "web": {
            "isOn": true,
            "redirectToHttps": {
                "isOn": true,
                "status": status,
                "host": "",
                "port": 0,
                "domains": [],
                "exceptDomains": []
            }
        }
    }))
    .expect("server config should parse");
    let plan = CompiledServerFeaturePlan::compile(&server).expect("feature plan compiles");
    let (_target, code) = plan
        .redirect_to_https_target("a.example", "/p?q=1")
        .expect("redirect should apply");
    code
}

#[test]
fn admin_console_statuses_are_emitted_verbatim() {
    for code in [301, 302, 303, 307, 308] {
        let got = effective_status(code);
        assert_eq!(got, code as u16, "status {code} must be preserved verbatim");
    }
}

#[test]
fn status_303_is_no_longer_coerced_to_301() {
    assert_eq!(
        effective_status(303),
        303,
        "303 (valid in the admin console) must take effect unchanged"
    );
}

#[test]
fn invalid_status_falls_back_to_301() {
    for invalid in [0, 200, 404, 999, -1] {
        assert_eq!(
            effective_status(invalid),
            301,
            "unlisted status {invalid} must fall back to 301"
        );
    }
}

#[test]
fn admin_console_still_offers_303() {
    // Guards the premise: 303 is a real, user-reachable value.
    //
    // The console module lives in another repository. A clean clone (CI) has no
    // sibling checkout, which made this test fail with ENOENT before it could
    // assert anything (run 39906686328) instead of testing the contract. When
    // the sibling checkout is absent the test falls back to a snapshot that
    // records the console revision it was captured from, and says so, so the
    // weaker check is visible rather than silent.
    const SNAPSHOT: &str = include_str!("fixtures/admin-console/site-settings-fixtures.ts");
    let sibling = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../admin-console/src/features/sites/site-settings-fixtures.ts"
    );
    let source = match std::fs::read_to_string(sibling) {
        Ok(live) => live,
        Err(err) => {
            eprintln!(
                "note: {sibling} unavailable ({err}); checking the recorded console contract instead"
            );
            SNAPSHOT.to_string()
        }
    };
    assert!(
        source.contains("301 | 302 | 303 | 307 | 308"),
        "admin console redirect status union must include 303"
    );
    assert!(
        source.contains("[301, 308, 302, 303, 307]"),
        "admin console redirect status picker must offer 303"
    );
}
