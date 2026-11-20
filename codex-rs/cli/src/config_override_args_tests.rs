//! Regression coverage for overrides split across CLI command boundaries.

use super::try_parse_from;
use crate::MultitoolCli;
use crate::Subcommand;
use crate::mcp_cmd::McpSubcommand;
use clap::Parser;
use clap::error::ErrorKind;
use pretty_assertions::assert_eq;

#[test]
fn preserves_scoped_overrides_and_forwarded_arguments() {
    let cli = try_parse_from([
        "codex",
        "--config=a=1",
        "mcp",
        "-cb=2",
        "add",
        "example",
        "-c",
        "a=3",
        "--",
        "program",
        "-c",
        "payload",
    ])
    .expect("parse");
    assert_eq!(cli.config_overrides.raw_overrides, ["a=1", "b=2", "a=3"]);
    let Some(Subcommand::Mcp(mcp)) = cli.subcommand else {
        panic!("expected mcp");
    };
    assert_eq!(mcp.config_overrides.raw_overrides, ["a=3"]);
    let McpSubcommand::Add(add) = mcp.subcommand else {
        panic!("expected add");
    };
    assert_eq!(
        add.transport_args.stdio.expect("stdio transport").command,
        ["program", "-c", "payload"],
    );

    let cli = try_parse_from(["codex", "exec", "--", "--credit-usage"])
        .expect("credit option text remains an exec prompt");
    let Some(Subcommand::Exec(exec)) = cli.subcommand else {
        panic!("expected exec");
    };
    assert_eq!(exec.prompt.as_deref(), Some("--credit-usage"));
}

#[test]
fn credit_only_account_edits_preserve_scoped_overrides_and_json() {
    let cli = try_parse_from([
        "codex",
        "-c",
        "a=1",
        "account",
        "edit",
        "alpha",
        "--credit-usage=disabled",
        "--json",
        "-c",
        "b=2",
    ])
    .expect("credit-only edit with config overrides");
    assert_eq!(cli.config_overrides.raw_overrides, ["a=1", "b=2"]);
    let Some(Subcommand::Account(account)) = cli.subcommand else {
        panic!("expected account");
    };
    assert!(account.json);
}

#[test]
fn account_edit_errors_retain_installed_golden_bytes() {
    let change = "<--priority <N>|--credit-usage <CREDIT_USAGE>|--note <TEXT>|--clear-note>";
    for json in [false, true] {
        let mut args = vec!["codex", "account", "edit", "alpha"];
        let json_usage = if json {
            args.push("--json");
            "--json "
        } else {
            ""
        };
        let missing =
            try_parse_from(args.iter().copied()).expect_err("a metadata change is required");
        assert_eq!(
            (missing.kind(), missing.to_string()),
            (
                ErrorKind::MissingRequiredArgument,
                format!(
                    "error: the following required arguments were not provided:\n  {change}\n\nUsage: codex account edit {json_usage}{change} <ACCOUNT>\n\nFor more information, try '--help'.\n"
                ),
            )
        );
        args.extend(["--note", "x", "--clear-note"]);
        let conflict = try_parse_from(args.iter().copied()).expect_err("conflicting note changes");
        assert_eq!(
            (conflict.kind(), conflict.to_string()),
            (
                ErrorKind::ArgumentConflict,
                format!(
                    "error: the argument '--note <TEXT>' cannot be used with '--clear-note'\n\nUsage: codex account edit {json_usage}{change} <ACCOUNT>\n\nFor more information, try '--help'.\n"
                ),
            )
        );
    }
}

#[test]
fn credit_edit_preserves_clap_errors_for_invalid_commands() {
    for args in [
        vec!["codex", "account", "edit", "--credit-usage", "enabled"],
        vec![
            "codex",
            "account",
            "edit",
            "alpha",
            "--credit-usage",
            "true",
        ],
        vec![
            "codex",
            "account",
            "edit",
            "alpha",
            "--credit-usage",
            "enabled",
            "--priority",
            "invalid",
        ],
        vec![
            "codex",
            "account",
            "edit",
            "alpha",
            "--credit-usage",
            "enabled",
            "--note",
            "x",
            "--clear-note",
        ],
        vec!["codex", "exec", "--model", "--credit-usage"],
    ] {
        let original =
            MultitoolCli::try_parse_from(args.iter().copied()).expect_err("invalid command");
        let actual =
            try_parse_from(args.iter().copied()).expect_err("retry cannot accept invalid commands");
        assert_eq!(
            (actual.kind(), actual.to_string()),
            (original.kind(), original.to_string()),
        );
    }
}
