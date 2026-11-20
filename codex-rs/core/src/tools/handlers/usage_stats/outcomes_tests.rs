use super::*;
use pretty_assertions::assert_eq;

const SNAPSHOT: &str = "0123456789abcdef0123456789abcdef";

fn report(rows: Vec<Value>) -> Value {
    json!({
        "kind":"performanceReview", "snapshotGeneration":7, "counts":{"operations":23},
        "outcomes":{"coverage":"fixture_only", "totalRows":rows.len(), "rows":rows,
            "nextCursor":null, "totals":{"operations":23}},
    })
}

#[test]
fn outcome_pages_respect_count_and_bytes_without_skipping_rows() {
    let rows = (0..53)
        .map(|index| json!({"id":format!("fixture-{index}"),"label":"x".repeat(/*n*/ 1_800)}))
        .collect::<Vec<_>>();
    let original = report(rows.clone());
    let mut collected = Vec::new();
    let mut offset = 0;
    loop {
        let page = page(&original, SNAPSHOT, offset, /*limit*/ 50).expect("bounded outcome page");
        let current = page["outcomes"]["rows"].as_array().expect("rows");
        assert!(!current.is_empty() && current.len() < 50);
        assert!(serde_json::to_vec(&page).expect("response").len() <= MAX_OUTPUT_BYTES);
        let mut rest = page.clone();
        let mut original_rest = original.clone();
        for value in [&mut rest, &mut original_rest] {
            value["outcomes"]
                .as_object_mut()
                .expect("outcomes")
                .remove("rows");
            value["outcomes"]
                .as_object_mut()
                .expect("outcomes")
                .remove("nextCursor");
        }
        assert_eq!(rest, original_rest);
        collected.extend_from_slice(current);
        let Some(next) = page["outcomes"]["nextCursor"].as_str() else {
            break;
        };
        let (snapshot, next_offset) = cursor(next).expect("continuation");
        assert_eq!(snapshot, SNAPSHOT);
        assert_eq!(next_offset, collected.len());
        offset = next_offset;
    }
    assert_eq!(collected, rows);
    let small = report(vec![json!({"id":"one"}), json!({"id":"two"})]);
    assert_eq!(
        page(&small, SNAPSHOT, /*offset*/ 0, /*limit*/ 1).expect("one row")["outcomes"]["rows"],
        json!([{"id":"one"}])
    );
}

#[test]
fn malformed_outcome_cursors_and_unavailable_offsets_are_rejected() {
    for value in [
        "",
        "usage-v1:other:1",
        "usage-outcomes-v1:bad:1",
        "usage-outcomes-v1:0123456789abcdef0123456789abcdef:0",
        "usage-outcomes-v1:0123456789abcdef0123456789abcdef:01",
        "usage-outcomes-v1:0123456789abcdef0123456789abcdef:-1",
        "usage-outcomes-v1:0123456789abcdef0123456789abcdef:18446744073709551616",
    ] {
        assert!(cursor(value).is_err(), "invalid cursor: {value}");
    }
    assert!(
        page(
            &report(vec![json!({"id":"one"})]),
            SNAPSHOT,
            /*offset*/ 1,
            /*limit*/ 1
        )
        .is_err()
    );
}

#[test]
fn an_oversized_outcome_is_rejected_instead_of_skipped() {
    let original = report(vec![json!({"label":"x".repeat(MAX_OUTPUT_BYTES)})]);
    assert!(page(&original, SNAPSHOT, /*offset*/ 0, /*limit*/ 8).is_err());
    let oversized = report(vec![json!({"label":"x".repeat(MAX_SNAPSHOT_BYTES)})]);
    assert!(page(&oversized, SNAPSHOT, /*offset*/ 0, /*limit*/ 8).is_err());
}
