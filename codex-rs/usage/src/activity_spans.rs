use crate::ActivitySpanId;
use crate::ActivityState;
use crate::OperationId;
use crate::UsageStore;
use crate::UsageStoreError;
use crate::facts::FactEventId;
use sqlx::Row;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivitySpanEventKind {
    Heartbeat,
    Ended,
}

impl ActivitySpanEventKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Heartbeat => "heartbeat",
            Self::Ended => "ended",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewActivitySpan {
    pub id: ActivitySpanId,
    pub operation_id: OperationId,
    pub activity_state: ActivityState,
    pub started_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewActivitySpanEvent {
    pub event_id: FactEventId,
    pub activity_span_id: ActivitySpanId,
    pub kind: ActivitySpanEventKind,
    pub occurred_at_ms: i64,
}

impl UsageStore {
    pub async fn begin_activity_span(&self, fact: &NewActivitySpan) -> Result<(), UsageStoreError> {
        if !matches!(
            fact.activity_state,
            ActivityState::ExternalWait | ActivityState::UserWait | ActivityState::BlockedWait
        ) {
            return Err(UsageStoreError::InvalidFact);
        }
        let result = sqlx::query(
            r#"
            INSERT INTO activity_spans(id, operation_id, activity_state, started_at_ms)
            VALUES (?, ?, ?, ?) ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(fact.id.as_string())
        .bind(fact.operation_id.as_string())
        .bind(fact.activity_state.as_str())
        .bind(fact.started_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 {
            let row = sqlx::query(
                "SELECT operation_id, activity_state, started_at_ms FROM activity_spans WHERE id = ?",
            )
            .bind(fact.id.as_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
            if !row.is_some_and(|row| {
                row.get::<String, _>("operation_id") == fact.operation_id.as_string()
                    && row.get::<String, _>("activity_state") == fact.activity_state.as_str()
                    && row.get::<i64, _>("started_at_ms") == fact.started_at_ms
            }) {
                return Err(UsageStoreError::FactConflict);
            }
        }
        Ok(())
    }

    pub async fn record_activity_span_event(
        &self,
        fact: &NewActivitySpanEvent,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO activity_span_events(
                event_id, activity_span_id, event_kind, occurred_at_ms
            ) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING
            "#,
        )
        .bind(fact.event_id.as_string())
        .bind(fact.activity_span_id.as_string())
        .bind(fact.kind.as_str())
        .bind(fact.occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 {
            let row = sqlx::query(
                r#"
                SELECT activity_span_id, event_kind, occurred_at_ms
                FROM activity_span_events WHERE event_id = ?
                "#,
            )
            .bind(fact.event_id.as_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
            if !row.is_some_and(|row| {
                row.get::<String, _>("activity_span_id") == fact.activity_span_id.as_string()
                    && row.get::<String, _>("event_kind") == fact.kind.as_str()
                    && row.get::<i64, _>("occurred_at_ms") == fact.occurred_at_ms
            }) {
                return Err(UsageStoreError::FactConflict);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "activity_spans_tests.rs"]
mod tests;
