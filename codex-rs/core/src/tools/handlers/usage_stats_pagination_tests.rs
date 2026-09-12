use super::*;
use pretty_assertions::assert_eq;

#[test]
fn task_tree_pages_preserve_totals_and_every_agent() {
    let agents = (0..143).map(|i| json!({
        "agentId": format!("agent-{i:03}"), "role": "worker", "operations": 3,
        "providerTotalTokens": {"measuredTokens":10,"exactTokens":10,"unknownObservations":0},
        "wallTime": {"measuredNs":1000,"exactNs":1000,"unknownIntervals":0}
    })).collect::<Vec<_>>();
    let original = json!({"kind":"taskTreeSummary","rootThreadId":"root","includeDescendants":true,
        "timeRange":{"startMs":0,"endMs":2000},"counts":{"agents":143},
        "totals":{"providerTotalTokens":{"measuredTokens":1430}},"agents":agents});
    let mut request = args();
    request.action = UsageStatsAction::TaskTreeSummary;
    let mut restored = Vec::new();
    loop {
        let mut page = original.clone();
        pagination::paginate(&mut page, &request).expect("page");
        bounded_output(page.clone()).expect("bounded model output");
        assert_eq!(page["totals"], original["totals"]);
        assert_eq!(page["counts"], original["counts"]);
        restored.extend(page["agents"].as_array().expect("agents").iter().cloned());
        let next = &page["pagination"]["nextCursor"];
        if next.is_null() {
            break;
        }
        request.cursor_id = Some(next["id"].as_str().expect("cursor id").to_string());
        request.cursor_sort_value = next["sortValue"].as_i64();
    }
    assert_eq!(restored, agents);
}

#[test]
fn summary_pages_cross_dimension_boundaries_and_reject_foreign_cursors() {
    let original = json!({"kind":"usageSummary","scope":{"type":"repository","id":"one"},
        "counts":{"operations":3},"providerTokensByActivity":[{"activity":"coding"},{"activity":"testing"}],
        "classifications":[{"activity":"coding"},{"activity":"testing"}]});
    let mut request = args();
    request.action = UsageStatsAction::Summary;
    request.limit = Some(3);
    let mut first = original.clone();
    pagination::paginate(&mut first, &request).expect("first page");
    assert_eq!(
        first["providerTokensByActivity"],
        original["providerTokensByActivity"]
    );
    assert_eq!(first["classifications"], json!([{"activity":"coding"}]));
    request.cursor_id = Some(
        first["pagination"]["nextCursor"]["id"]
            .as_str()
            .expect("id")
            .to_string(),
    );
    request.cursor_sort_value = first["pagination"]["nextCursor"]["sortValue"].as_i64();
    let mut second = original.clone();
    pagination::paginate(&mut second, &request).expect("second page");
    assert_eq!(second["classifications"], json!([{"activity":"testing"}]));
    assert_eq!(second["pagination"]["nextCursor"], Value::Null);
    let mut foreign = original;
    foreign["scope"]["id"] = json!("two");
    assert!(pagination::paginate(&mut foreign, &request).is_err());
}
