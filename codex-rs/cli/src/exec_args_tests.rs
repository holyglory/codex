//! Snapshot coverage for the public exec command's argument help.

use super::MultitoolCli;
use clap::CommandFactory;
use clap::Parser;
use pretty_assertions::assert_eq;

#[test]
fn exec_help_documents_cyber_access_program() {
    let help = MultitoolCli::command()
        .term_width(80)
        .try_get_matches_from(["codex", "exec", "--help"])
        .expect_err("help should short-circuit");
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    let program_help = help
        .to_string()
        .lines()
        .skip_while(|line| !line.contains("--cyber-access-program"))
        .take_while(|line| !line.is_empty())
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(program_help.trim(), @r"
    --cyber-access-program <PROGRAM>
              Request an experimental Cyber access program for this turn (OpenAI
              provider only). Omit to use server defaults. Not supported with
              review; fork requires a prompt

              [possible values: standard, daybreak_blue, daybreak_red]
    ");
}

#[test]
fn account_pin_parses_for_interactive_exec_resume_and_app_server() {
    for argv in [
        vec!["codex", "--account", "local"],
        vec!["codex", "exec", "--account", "local", "hello"],
        vec!["codex", "exec", "resume", "--account", "local", "--last"],
        vec!["codex", "app-server", "--account", "local"],
    ] {
        MultitoolCli::try_parse_from(argv).expect("local account pin should parse");
    }
}
