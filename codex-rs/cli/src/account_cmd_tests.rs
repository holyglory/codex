use clap::CommandFactory;
use clap::Parser;
use pretty_assertions::assert_eq;

use super::AccountAction;
use super::AccountCommand;
use super::AutoAction;
use super::confirmation_accepted;

#[test]
fn parser_exposes_only_implemented_account_commands() {
    let command = AccountCommand::command();
    let names = command
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "list", "current", "show", "add", "limits", "reset", "rename", "edit", "priority",
            "use", "enable", "disable", "remove", "auto", "doctor",
        ]
    );
}

#[test]
fn parser_accepts_json_after_nested_auto_action() {
    let parsed = AccountCommand::try_parse_from(["account", "auto", "on", "--json"])
        .expect("implemented account syntax should parse");
    assert!(parsed.json);
    assert!(matches!(
        parsed.action,
        AccountAction::Auto(super::AutoArgs {
            action: Some(AutoAction::On(_)),
            ..
        })
    ));
}

#[test]
fn parser_requires_an_explicit_credit_usage_mode() {
    for (value, expected) in [
        ("enabled", super::CreditUsageMode::Enabled),
        ("disabled", super::CreditUsageMode::Disabled),
    ] {
        for changes in [vec![], vec!["--priority", "42", "--note", "updated"]] {
            let parsed = crate::config_override_args::try_parse_from(
                ["codex", "account", "edit", "alpha", "--credit-usage", value]
                    .into_iter()
                    .chain(changes.iter().copied()),
            )
            .expect("explicit credit permission through the CLI entrypoint");
            let Some(crate::Subcommand::Account(parsed)) = parsed.subcommand else {
                panic!("expected account command");
            };
            let AccountAction::Edit(args) = parsed.action else {
                panic!("expected account edit");
            };
            let combined = !changes.is_empty();
            assert_eq!(
                (args.credit_usage, args.priority, args.note),
                (
                    Some(expected),
                    combined.then_some(/*t*/ 42),
                    combined.then(|| "updated".to_string()),
                )
            );
        }
    }
    for invalid in ["true", "1", "ENABLED"] {
        assert!(
            crate::config_override_args::try_parse_from([
                "codex",
                "account",
                "edit",
                "alpha",
                "--credit-usage",
                invalid,
            ])
            .is_err()
        );
    }
    assert!(
        crate::config_override_args::try_parse_from([
            "codex",
            "account",
            "edit",
            "alpha",
            "--credit-usage",
        ])
        .is_err()
    );
}

#[test]
fn parser_requires_credit_id_when_replaying_reset() {
    assert!(
        AccountCommand::try_parse_from(["account", "reset", "alpha", "--request-id", "req"])
            .is_err()
    );
    assert!(
        AccountCommand::try_parse_from([
            "account",
            "reset",
            "alpha",
            "--credit-id",
            "credit",
            "--request-id",
            "req"
        ])
        .is_ok()
    );
}

#[test]
fn confirmation_is_explicit_and_conservative() {
    for accepted in ["y", "Y", "yes", " YES "] {
        assert!(confirmation_accepted(accepted));
    }
    for rejected in ["", "n", "no", "true", "1"] {
        assert!(!confirmation_accepted(rejected));
    }
}
