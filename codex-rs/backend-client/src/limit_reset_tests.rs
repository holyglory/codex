use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn quota(id: &str, primary: f64, secondary: f64) -> RateLimitSnapshot {
    serde_json::from_value(json!({
        "limit_id": id,
        "primary": {"used_percent": primary, "window_minutes": 300, "resets_at": 200},
        "secondary": {"used_percent": secondary, "window_minutes": 10080, "resets_at": 900}
    }))
    .unwrap()
}

#[test]
fn reset_uses_main_codex_and_prioritizes_exhausted_then_used_windows() {
    for (primary, secondary, scope, resets_at) in [
        (10.0, 100.0, "codex.secondary", 900),
        (100.0, 10.0, "codex.primary", 200),
        (100.0, 100.0, "codex.primary", 200),
        (0.0, 61.0, "codex.secondary", 900),
        (42.0, 61.0, "codex.primary", 200),
        (0.0, 0.0, "codex.primary", 200),
    ] {
        let snapshots = [
            quota("codex_bengalfox", 0.0, 0.0),
            quota("codex", primary, secondary),
        ];
        assert_eq!(
            next_codex_limit_reset(&snapshots, /*observed_at*/ 100),
            Some(CodexLimitReset { resets_at, scope }),
        );
    }
}

#[test]
fn missing_main_quota_or_blocked_reset_stays_unknown() {
    let spark = quota("codex_bengalfox", 0.0, 0.0);
    assert_eq!(
        next_codex_limit_reset(std::slice::from_ref(&spark), /*observed_at*/ 100),
        None
    );
    for reset in [None, Some(99), Some(100)] {
        let mut codex = quota("codex", 10.0, 100.0);
        codex.secondary.as_mut().unwrap().resets_at = reset;
        assert_eq!(
            next_codex_limit_reset(&[spark.clone(), codex], /*observed_at*/ 100),
            None
        );
    }
}

#[test]
fn individual_exhaustion_uses_its_own_reset() {
    let mut codex = quota("codex", 42.0, 61.0);
    codex.individual_limit = Some(
        serde_json::from_value(json!({
            "limit": "25000", "used": "25000", "remaining_percent": 0, "resets_at": 800
        }))
        .unwrap(),
    );
    assert_eq!(
        next_codex_limit_reset(&[codex], /*observed_at*/ 100),
        Some(CodexLimitReset {
            resets_at: 800,
            scope: "codex.individual"
        })
    );
}
