"""Keep downstream capacity recovery in every shipped package's acceptance run."""

import pytest
from app_server_harness import MockResponsesServer, ev_response_created, sse
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage


@pytest.mark.parametrize("entrypoint", ["codex", "codex-app-server"])
@pytest.mark.parametrize("error_code", ["server_is_overloaded", "slow_down"])
def test_capacity_response_recovers_in_same_turn(
    package: SmokePackage,
    responses_server: MockResponsesServer,
    entrypoint: str,
    error_code: str,
) -> None:
    """A provider failure after response.created retries without failing the task."""
    responses_server.enqueue_sse(
        sse(
            [
                ev_response_created("capacity-pending"),
                {
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-pending",
                        "error": {"code": error_code, "message": "Temporary overload"},
                    },
                },
            ]
        )
    )
    responses_server.enqueue_assistant_message("RECOVERED", response_id="recovered")
    launch = (
        (str(package.cli), "app-server")
        if entrypoint == "codex"
        else (str(package.app_server),)
    )
    config = CodexConfig(
        launch_args_override=launch,
        cwd=str(package.directory),
        env=package.environment,
    )
    with Codex(config=config) as client:
        thread = client.thread_start(
            ephemeral=True,
            approval_mode=ApprovalMode.deny_all,
            sandbox=Sandbox.read_only,
        )
        events = list(thread.turn("Recover this pending request").stream())
    completed = [
        event.payload.turn for event in events if event.method == "turn/completed"
    ]
    assert len(completed) == 1
    assert completed[0].status.value == "completed", completed[0].error
    retries = [event.payload for event in events if event.method == "error"]
    assert len(retries) == 1 and retries[0].will_retry
    assert retries[0].error.message == "Reconnecting... 1/5"
    requests = responses_server.requests()
    assert len(requests) == 2
    assert requests[0].input() == requests[1].input()
