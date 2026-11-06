//! Preserve config overrides across command boundaries without changing typed child arguments.

use crate::MultitoolCli;
use clap::CommandFactory;
use clap::FromArgMatches;
use codex_utils_cli::collect_config_overrides;
use std::ffi::OsString;

pub(super) fn try_parse_from(
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<MultitoolCli, clap::Error> {
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let mut command = MultitoolCli::command();
    let mut credit_edit_command = None;
    let mut matches = match command.try_get_matches_from_mut(&args) {
        Ok(matches) => matches,
        Err(error)
            if error.kind() == clap::error::ErrorKind::MissingRequiredArgument
                && args.iter().any(|arg| {
                    arg == "--credit-usage"
                        || arg
                            .to_str()
                            .is_some_and(|arg| arg.starts_with("--credit-usage="))
                }) =>
        {
            // Keep legacy help and parser errors intact. Only a successful,
            // explicit account/edit credit change may extend the required group.
            let extended = MultitoolCli::command().mut_subcommand("account", |account| {
                account.mut_subcommand("edit", |edit| {
                    edit.mut_group("change", |group| group.arg("credit_usage"))
                })
            });
            // Retain an unbuilt copy for scoped config-override collection.
            let mut retry = extended.clone();
            match retry.try_get_matches_from_mut(&args) {
                Ok(matches)
                    if matches
                        .subcommand_matches("account")
                        .and_then(|account| account.subcommand_matches("edit"))
                        .is_some_and(|edit| {
                            edit.value_source("credit_usage")
                                == Some(clap::parser::ValueSource::CommandLine)
                        }) =>
                {
                    credit_edit_command = Some(extended);
                    command = retry;
                    matches
                }
                Ok(_) | Err(_) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    crate::queue_cmd::validate_options(&command, &matches)?;
    let mut cli = MultitoolCli::from_arg_matches_mut(&mut matches)
        .map_err(|error| error.format(&mut command))?;
    if cli.subcommand.is_none() || cli.config_overrides.raw_overrides.is_empty() {
        return Ok(cli);
    }

    // Keep child copies unchanged: dispatch merges apply them after feature overrides.
    cli.config_overrides = collect_config_overrides(
        credit_edit_command.unwrap_or_else(MultitoolCli::command),
        args,
    )?;
    Ok(cli)
}

#[cfg(test)]
#[path = "config_override_args_tests.rs"]
mod tests;
