DROP TRIGGER token_observations_hosted_tool_source;

CREATE TRIGGER token_observations_hosted_tool_source BEFORE INSERT ON token_observations
WHEN NEW.tool_invocation_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM tool_invocations AS tool
    WHERE tool.id = NEW.tool_invocation_id
      AND (
          (
              tool.operation_kind = 'local_tool'
              AND tool.covering_model_request_id IS NULL
          ) OR (
              tool.operation_kind = 'hosted_tool'
              AND tool.covering_model_request_id IS NOT NULL
          )
      )
)
BEGIN SELECT RAISE(ABORT, 'token tool source lacks valid provider coverage'); END;
