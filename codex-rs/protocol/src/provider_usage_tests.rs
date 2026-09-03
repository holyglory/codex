use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[test]
fn preserves_absent_null_zero_invalid_and_full_u64_values() {
    let usage = ProviderUsage::from_json_value(&json!({
        "input_tokens": 0,
        "input_tokens_details": {"cached_tokens": null},
        "output_tokens": u64::MAX
    }));

    assert_eq!(usage.input_tokens(), ProviderTokenCount::Value(0));
    assert_eq!(usage.cached_input_tokens(), ProviderTokenCount::Null);
    assert_eq!(usage.cache_write_input_tokens(), ProviderTokenCount::Absent);
    assert_eq!(usage.output_tokens(), ProviderTokenCount::Value(u64::MAX));
    assert_eq!(usage.total_tokens(), ProviderTokenCount::Absent);
    assert!(usage.categories_complete());

    let overflow: ProviderUsage =
        serde_json::from_str(r#"{"input_tokens":18446744073709551616,"output_tokens":-1}"#)
            .expect("overflow fixture should remain a JSON number");
    assert_eq!(overflow.input_tokens(), ProviderTokenCount::Invalid);
    assert_eq!(overflow.output_tokens(), ProviderTokenCount::Invalid);
    assert!(!overflow.categories_complete());
}

#[test]
fn preserves_valid_future_categories_and_invalid_presence() {
    let usage = ProviderUsage::from_json_value(&json!({
        "input_tokens_details": {
            "audio_tokens": null,
            "image_tokens": 10,
            "text_tokens": 0,
            "fractional_tokens": 2.5
        },
        "request_count": 1
    }));

    assert_eq!(
        usage.additional_token_categories(),
        &BTreeMap::from([
            (
                "input_tokens_details.audio_tokens".to_string(),
                ProviderTokenCount::Null,
            ),
            (
                "input_tokens_details.fractional_tokens".to_string(),
                ProviderTokenCount::Invalid,
            ),
            (
                "input_tokens_details.image_tokens".to_string(),
                ProviderTokenCount::Value(10),
            ),
            (
                "input_tokens_details.text_tokens".to_string(),
                ProviderTokenCount::Value(0),
            ),
        ])
    );
    assert!(!usage.categories_complete());
    assert_eq!(
        usage.category_schema_version(),
        PROVIDER_USAGE_CATEGORY_SCHEMA_VERSION
    );
}

#[test]
fn unrelated_invalid_and_deep_metadata_do_not_create_false_partial_coverage() {
    let usage = ProviderUsage::from_json_value(&json!({
        "request-metadata": 1,
        "metadata": {"one": {"two": {"three": {"four": {"value": 9}}}}},
        "request_count": 1
    }));

    assert!(usage.additional_token_categories().is_empty());
    assert!(usage.categories_complete());

    let hidden = ProviderUsage::from_json_value(&json!({
        "request-metadata": {"nested": {"image_tokens": 3}}
    }));
    assert!(!hidden.categories_complete());
}

#[test]
fn ignores_opaque_per_item_attribution_breakdowns() {
    let usage = ProviderUsage::from_json_value(&json!({
        "input_tokens": 12,
        "output_tokens": 3,
        "total_tokens": 15,
        "attribution": {
            "items": {
                "msg_opaque_provider_id": {
                    "input_tokens": 12,
                    "output_tokens": 3
                }
            }
        }
    }));

    assert_eq!(usage.input_tokens(), ProviderTokenCount::Value(12));
    assert_eq!(usage.output_tokens(), ProviderTokenCount::Value(3));
    assert_eq!(usage.total_tokens(), ProviderTokenCount::Value(15));
    assert!(usage.additional_token_categories().is_empty());
    assert!(usage.categories_complete());
}

#[test]
fn rejects_unsafe_token_keys_without_retaining_them() {
    let usage = ProviderUsage::from_json_value(&json!({
        "Prompt text_tokens": 200,
        "output_tokens_details": {"negative_tokens": -4}
    }));

    assert_eq!(
        usage.additional_token_categories(),
        &BTreeMap::from([(
            "output_tokens_details.negative_tokens".to_string(),
            ProviderTokenCount::Invalid,
        )])
    );
    assert!(!usage.categories_complete());
}

#[test]
fn caps_retained_categories_and_total_traversal_nodes() {
    let mut token_details = serde_json::Map::new();
    for index in 0..=MAX_ADDITIONAL_TOKEN_CATEGORIES {
        token_details.insert(format!("category_{index:02}_tokens"), json!(index));
    }
    let category_capped = ProviderUsage::from_json_value(&Value::Object(
        [("details".to_string(), Value::Object(token_details))]
            .into_iter()
            .collect(),
    ));
    assert_eq!(
        category_capped.additional_token_categories().len(),
        MAX_ADDITIONAL_TOKEN_CATEGORIES
    );
    assert!(!category_capped.categories_complete());

    let mut wide = serde_json::Map::new();
    for index in 0..=MAX_USAGE_TRAVERSAL_NODES {
        wide.insert(format!("metadata_{index}"), json!(index));
    }
    let traversal_capped = ProviderUsage::from_json_value(&Value::Object(wide));
    assert!(traversal_capped.additional_token_categories().is_empty());
    assert!(!traversal_capped.categories_complete());
}

#[test]
fn source_event_key_is_stable_distinct_and_debug_redacted() {
    let first = ProviderSourceEventKey::from_provider_response_id("response-sensitive-one")
        .expect("nonempty response id");
    let replay = ProviderSourceEventKey::from_provider_response_id("response-sensitive-one")
        .expect("nonempty response id");
    let second = ProviderSourceEventKey::from_provider_response_id("response-sensitive-two")
        .expect("nonempty response id");

    assert_eq!(first, replay);
    assert_ne!(first, second);
    assert!(!format!("{first:?}").contains("response-sensitive-one"));
    assert_eq!(ProviderSourceEventKey::from_provider_response_id(""), None);
}
