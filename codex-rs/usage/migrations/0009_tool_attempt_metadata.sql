ALTER TABLE tool_invocations ADD COLUMN interface TEXT NOT NULL DEFAULT 'unknown'
    CHECK (length(interface) BETWEEN 1 AND 64);

ALTER TABLE tool_invocations ADD COLUMN request_shape TEXT
    CHECK (request_shape IS NULL OR length(request_shape) BETWEEN 2 AND 4096);

ALTER TABLE operation_events ADD COLUMN failure_stage TEXT CHECK (
    failure_stage IS NULL OR failure_stage IN (
        'unknown_tool', 'invalid_payload', 'pre_tool_hook', 'approval',
        'handler', 'post_tool_hook', 'cancellation', 'interruption',
        'persistence', 'unknown'
    )
);

ALTER TABLE operation_events ADD COLUMN error_code TEXT CHECK (
    error_code IS NULL OR length(error_code) BETWEEN 1 AND 128
);
