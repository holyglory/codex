"""Keep downstream capacity recovery in every shipped package's acceptance run."""

import json
import subprocess

import pytest
from app_server_harness import MockResponsesServer, ev_response_created, sse
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox

from fixtures import SmokePackage


@pytest.mark.parametrize("entrypoint", ["codex", "codex-app-server", "exec"])
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
    if entrypoint == "exec":
        output = package.run(
            "exec", "--skip-git-repo-check", "--json", "Recover this pending request"
        ).stdout
        events = [json.loads(line) for line in output.splitlines() if line.strip()]
        assert sum(event["type"] == "turn.completed" for event in events) == 1
        assert "RECOVERED" in output
        assert all(event["type"] != "error" for event in events)
        thread_id = next(
            event["thread_id"] for event in events if event["type"] == "thread.started"
        )
    else:
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
        assert retries == []
        thread_id = thread.id
    requests = responses_server.requests()
    assert len(requests) == 2
    assert requests[0].input() == requests[1].input()

    # Query persisted evidence through the packaged CLI after its server exits.
    # This also keeps the public diagnostics command in the release inventory.
    incidents = json.loads(
        package.run("debug", "network-errors", "--thread-id", thread_id).stdout
    )
    assert any(
        row["event"] == "provider_stream_error"
        and row["details"].get("error_code") == error_code
        for row in incidents["data"]
    ), incidents
    assert all(row["threadId"] == thread_id for row in incidents["data"])
    assert "Recover this pending request" not in json.dumps(incidents)


def test_exec_network_failure_is_readable_after_exit(
    package: SmokePackage,
    responses_server: MockResponsesServer,
) -> None:
    """A final failed CLI turn retains its transport cause after process exit."""
    responses_server.close()
    with pytest.raises(subprocess.CalledProcessError) as failed:
        package.run(
            "exec",
            "--skip-git-repo-check",
            "--json",
            "-c",
            "features.unbounded_connection_retries=false",
            "Private failed CLI fixture",
        )
    events = [
        json.loads(line) for line in failed.value.stdout.splitlines() if line.strip()
    ]
    thread_id = next(
        event["thread_id"] for event in events if event["type"] == "thread.started"
    )
    incidents = json.loads(
        package.run("debug", "network-errors", "--thread-id", thread_id).stdout
    )
    assert any(row["event"] == "http_transport_failure" for row in incidents["data"])
    assert any(
        "refused" in row["details"].get("error", "").lower()
        for row in incidents["data"]
    ), incidents
    assert "Private failed CLI fixture" not in json.dumps(incidents)
