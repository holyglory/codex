use super::*;

#[tokio::test]
async fn scoped_review_keeps_covered_facts_and_excludes_other_token_owners() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    let request = fixture.model(&model).await;
    fixture.finish(&model, /*duration_ms*/ 100).await;
    let event = FactEventId::new();
    fixture.token(TokenObservationSource::ModelRequest(request), event, "total_tokens", /*count*/ 100).await;
    let mut hosted = fixture.operation(/*started_at_ms*/ 1_000_010);
    hosted.kind = OperationKind::HostedTool;
    hosted.thread_id = None;
    let tool = fixture.tool(&hosted, ToolExecutionRole::Standalone, /*group*/ None, Some(request)).await;
    fixture.token(TokenObservationSource::ToolInvocation(tool), event, "total_tokens", /*count*/ 100).await;
    let query = PerformanceReviewQuery {
        thread_id: Some(ThreadId::new("spec").expect("thread")),
        repository_id: Some(fixture.repository.clone()),
        time_range: Some(UtcTimeRange::new(1_000_000, 1_001_000).expect("range")),
    };
    let expected = fixture.store.performance_review_packet(query.clone()).await.expect("review");
    assert_eq!(
        (expected.tokens[0].measured_tokens, expected.coverage.raw_token_observations,
         expected.coverage.deduplicated_token_observations),
        (100, 2, 1)
    );
    for _ in 0..16 {
        let mut unrelated = fixture.operation(/*started_at_ms*/ 1_000_050);
        unrelated.kind = OperationKind::ModelRequest;
        unrelated.thread_id = None;
        let request = fixture.model(&unrelated).await;
        fixture.token(TokenObservationSource::ModelRequest(request), FactEventId::new(), "total_tokens", /*count*/ 1_000).await;
        let mut tool = fixture.operation(/*started_at_ms*/ 1_000_060);
        tool.kind = OperationKind::HostedTool;
        tool.thread_id = None;
        let tool = fixture.tool(&tool, ToolExecutionRole::Standalone, /*group*/ None, Some(request)).await;
        fixture.token(TokenObservationSource::ToolInvocation(tool), FactEventId::new(), "total_tokens", /*count*/ 2_000).await;
    }
    assert_eq!(fixture.store.performance_review_packet(query).await.expect("review"), expected);
}
