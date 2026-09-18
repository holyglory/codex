use super::NetworkDiagnostic;
use codex_secrets::redact_network_diagnostic;
use serde_json::Value;
use std::collections::BTreeMap;
use tracing::Event;
use tracing::field::Field;
use tracing::field::Visit;
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone, Default)]
pub(crate) struct NetworkFields(pub(crate) BTreeMap<String, Value>);

impl Visit for NetworkFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field_name(field.name()).is_none() {
            return;
        }
        let limit = if matches!(field.name(), "error.message" | "close_reason") {
            8192
        } else {
            512
        };
        let origin_only = field.name() == "origin"
            && value.split_once("://").is_some_and(|(scheme, authority)| {
                matches!(scheme, "http" | "https" | "ws" | "wss")
                    && !authority.is_empty()
                    && authority.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
                    })
            });
        let safe = if origin_only {
            value.to_string()
        } else {
            redact_network_diagnostic(value)
        };
        let bounded = if safe.chars().count() > limit {
            format!(
                "{} [truncated]",
                safe.chars().take(limit).collect::<String>()
            )
        } else {
            safe
        };
        self.insert(field.name(), Value::String(bounded));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field.name(), value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field.name(), value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field.name(), value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field_name(field.name()).is_some() {
            self.record_str(field, &format!("{value:?}"));
        }
    }
}

impl NetworkFields {
    pub(crate) fn retain_context(&mut self) {
        self.0.retain(|name, _| {
            matches!(
                name.as_str(),
                "thread_id"
                    | "turn_id"
                    | "model"
                    | "transport"
                    | "endpoint"
                    | "request_id"
                    | "warmup"
                    | "connection_reused"
            )
        });
    }

    fn insert(&mut self, name: &str, value: Value) {
        if let Some(name) = field_name(name) {
            self.0.insert(name.to_string(), value);
        }
    }

    pub(crate) fn event<S>(
        event: &Event<'_>,
        ctx: &tracing_subscriber::layer::Context<'_, S>,
    ) -> Option<NetworkDiagnostic>
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        let target = event.metadata().target();
        if !matches!(
            target,
            "codex_otel.trace_safe"
                | "codex.network_diagnostics"
                | "codex_core::responses_retry"
                | "codex_core::client"
        ) {
            return None;
        }
        let mut fields = Self::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(context) = span.extensions().get::<Self>() {
                    fields.0.extend(context.0.clone());
                }
            }
        }
        event.record(&mut fields);
        let name = match target {
            "codex_otel.trace_safe" => match fields.0.get("event").and_then(Value::as_str)? {
                "codex.api_request"
                | "codex.websocket_connect"
                | "codex.websocket_request"
                | "codex.websocket_error"
                | "codex.sse_event"
                | "codex.auth_recovery" => fields.0.get("event")?.as_str()?.to_string(),
                _ => return None,
            },
            "codex.network_diagnostics" => fields.0.get("event")?.as_str()?.to_string(),
            "codex_core::responses_retry" => "retry".to_string(),
            "codex_core::client"
                if fields.0.get("message").and_then(Value::as_str)
                    == Some("falling back to HTTP") =>
            {
                "fallback_to_http".to_string()
            }
            _ => return None,
        };
        // SSE telemetry can contain an upstream JSON error body. Keep the event
        // classification; never retain that body or arbitrary log prose.
        if name == "codex.sse_event" {
            let failed = fields.0.remove("error").is_some();
            fields.0.insert("failed".to_string(), failed.into());
        }
        if let Some(message) = fields.0.remove("message")
            && name == "retry"
        {
            fields.0.insert("retry_schedule".to_string(), message);
        }
        fields.0.insert("source".to_string(), target.into());
        fields.0.insert(
            "level".to_string(),
            event.metadata().level().as_str().into(),
        );
        Some(NetworkDiagnostic {
            id: 0,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            thread_id: fields
                .0
                .get("thread_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            turn_id: fields
                .0
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            event: name,
            details: fields.0,
        })
    }
}

fn field_name(name: &str) -> Option<&str> {
    match name {
        "thread_id" | "thread.id" | "conversation.id" => Some("thread_id"),
        "turn_id" | "turn.id" => Some("turn_id"),
        "event.name" | "event" => Some("event"),
        "event.kind" => Some("kind"),
        "api.path" => Some("endpoint"),
        "error.message" => Some("error"),
        "http.response.status_code" => Some("http_status"),
        "auth.request_id" => Some("request_id"),
        "auth.cf_ray" => Some("cf_ray"),
        "auth.error_code" => Some("auth_error_code"),
        "auth.header_attached" => Some("auth_header_attached"),
        "auth.header_name" => Some("auth_header_name"),
        "auth.connection_reused" => Some("connection_reused"),
        "auth.retry_after_unauthorized" => Some("retry_after_unauthorized"),
        "auth.recovery_mode" | "auth.mode" => Some("recovery_mode"),
        "auth.recovery_phase" | "auth.step" => Some("recovery_phase"),
        "auth.outcome" => Some("recovery_outcome"),
        "message" | "model" | "app.version" | "endpoint" | "origin" | "http_status"
        | "error_code" | "duration_ms" | "attempt" | "success" | "retries" | "max_retries"
        | "retry_delay" | "close_code" | "close_reason" | "request_id" | "cf_ray" | "kind"
        | "transport" | "elapsed_ms" | "last_event" | "response_id" | "connection_reused"
        | "warmup" | "request_bytes" | "batch_limit_bytes" | "batch_index" | "input_items" => {
            Some(name)
        }
        _ => None,
    }
}
