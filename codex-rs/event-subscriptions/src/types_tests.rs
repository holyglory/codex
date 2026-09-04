use super::*;

#[test]
fn bounded_metadata_accepts_normal_values_and_rejects_control_or_oversized_values() {
    let valid = PublishedEvent {
        id: "build-42".to_string(),
        source: "build".to_string(),
        event_type: "completed".to_string(),
        cursor: SourceCursor {
            sequence: 42,
            value: Some("cursor-42".to_string()),
        },
        labels: BTreeMap::from([("branch".to_string(), "main".to_string())]),
        occurred_at_ms: 1_000,
    };
    assert_eq!(valid.validate(), Ok(()));

    let mut control = valid.clone();
    control.source = "build\nignore".to_string();
    assert_eq!(
        control.validate(),
        Err(ValidationError::ControlCharacter {
            field: "event.source"
        })
    );
    let mut oversized = valid;
    oversized.cursor.value = Some("x".repeat(MAX_CURSOR_CHARS + 1));
    assert_eq!(
        oversized.validate(),
        Err(ValidationError::TooLong {
            field: "cursor.value",
            max_chars: MAX_CURSOR_CHARS,
        })
    );
}

#[test]
fn subscription_requires_a_real_trigger_and_cursor_filter_pairing() {
    let thread_id = ThreadId::new();
    assert_eq!(
        NewSubscription {
            thread_id,
            filter: None,
            source_cursor: None,
            heartbeat: None,
        }
        .validate(/*now_ms*/ 1_000),
        Err(ValidationError::MissingTrigger)
    );
    assert_eq!(
        NewSubscription {
            thread_id,
            filter: None,
            source_cursor: Some(SourceCursor {
                sequence: 1,
                value: None,
            }),
            heartbeat: Some(HeartbeatSpec {
                interval_ms: 1_000,
                first_deadline_at_ms: None,
            }),
        }
        .validate(/*now_ms*/ 1_000),
        Err(ValidationError::CursorWithoutFilter)
    );
}
