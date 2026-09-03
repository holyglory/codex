use super::*;
use crate::provider_usage::ProviderUsage;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn image_response_retains_presence_preserving_usage() {
    let usage = json!({
        "input_tokens": 20,
        "input_tokens_details": {
            "image_tokens": 18,
            "text_tokens": 2
        },
        "output_tokens": 10,
        "output_tokens_details": {
            "image_tokens": 10,
            "text_tokens": 0
        },
        "total_tokens": 30
    });
    let response: ImageResponse = serde_json::from_value(json!({
        "created": 1,
        "data": [{"b64_json": "fixture"}],
        "usage": usage
    }))
    .expect("valid image response");

    assert_eq!(
        response,
        ImageResponse {
            created: 1,
            data: vec![ImageData {
                b64_json: "fixture".to_string(),
            }],
            background: None,
            quality: None,
            size: None,
            usage: Some(
                serde_json::from_value::<ProviderUsage>(usage).expect("valid image usage fixture"),
            ),
        }
    );
}
