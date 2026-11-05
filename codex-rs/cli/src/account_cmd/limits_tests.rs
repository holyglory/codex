use super::reset_countdown;
use pretty_assertions::assert_eq;

#[test]
fn reset_countdown_handles_unknown_expired_and_unit_boundaries() {
    let offsets = [
        None,
        Some(-1),
        Some(0),
        Some(59),
        Some(60),
        Some(3599),
        Some(3600),
        Some(86399),
        Some(86400),
        Some(500940),
    ];
    assert_eq!(
        offsets
            .map(|offset| reset_countdown(offset.map(|seconds| 1000 + seconds), /*now*/ 1000)),
        [
            "unknown",
            "now",
            "now",
            "<1m",
            "1m",
            "59m",
            "1h 0m",
            "23h 59m",
            "1d 0h 0m",
            "5d 19h 9m"
        ]
        .map(str::to_string)
    );
}
