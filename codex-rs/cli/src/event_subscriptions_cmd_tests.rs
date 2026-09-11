use super::*;

#[test]
fn wake_policy_scopes_require_explicit_permission_and_revision() {
    assert!(
        EventSubscriptionsCommand::try_parse_from([
            "subscriptions",
            "wake-policy",
            "--thread",
            "task"
        ])
        .is_ok()
    );
    assert!(
        EventSubscriptionsCommand::try_parse_from([
            "subscriptions",
            "wake-policy",
            "--thread",
            "task",
            "--policy",
            "allow-background"
        ])
        .is_err()
    );
    for scope in [
        vec![],
        vec!["--subscription", "alarm"],
        vec!["--project", "project", "--target", "preview"],
        vec!["--project", "project", "--review"],
    ] {
        let mut args = vec![
            "subscriptions",
            "wake-policy",
            "--thread",
            "task",
            "--policy",
            "allow-background",
            "--expected-revision",
            "1",
            "--authorization-ref",
            "user-request",
        ];
        args.extend(scope);
        assert!(EventSubscriptionsCommand::try_parse_from(args).is_ok());
    }
}

#[test]
fn labels_are_structured_and_duplicates_are_rejected() {
    assert_eq!(
        parse_labels(vec!["repo=codex".to_string(), "state=ready".to_string()]).unwrap(),
        BTreeMap::from([
            ("repo".to_string(), "codex".to_string()),
            ("state".to_string(), "ready".to_string()),
        ])
    );
    assert!(parse_labels(vec!["repo=a".to_string(), "repo=b".to_string()]).is_err());
    assert!(parse_labels(vec!["missing-separator".to_string()]).is_err());
}

#[test]
fn clock_only_and_event_filters_remain_distinct() {
    assert_eq!(
        create_filter(/*source*/ None, Vec::new(), Vec::new()).unwrap(),
        None
    );
    assert_eq!(
        create_filter(
            Some("build".to_string()),
            vec!["completed".to_string()],
            vec!["branch=main".to_string()],
        )
        .unwrap(),
        Some(EventSubscriptionFilter {
            source: "build".to_string(),
            event_types: vec!["completed".to_string()],
            labels: BTreeMap::from([("branch".to_string(), "main".to_string())]),
        })
    );
    assert!(
        create_filter(
            /*source*/ None,
            vec!["completed".to_string()],
            Vec::new(),
        )
        .is_err()
    );
}
