//! `reef schema` (R2.24, ADR 0050): the commands, the MCP tools and the API
//! as one JSON Schema 2020-12 document ([`crate::schema`]), or the API's
//! descriptor set, so a harness generates tool definitions rather than a
//! person writing them. Offline, like `guide`: it reads no configuration
//! and connects to no daemon, and describes the binary that runs.
//!
//! This module also builds the document's `cli` part: each leaf command's
//! input schema and `argv` from clap ([`commands`]), and its output in
//! each form from [`outputs::OUTPUTS`].

mod commands;
pub(crate) mod outputs;

use reef_error::{Advice, ErrorKind};
use schemars::SchemaGenerator;
use serde_json::{Value, json};

use super::{CliError, Out, Output};

pub(crate) use commands::root as commands_root;
use commands::{global, leaf_command};

/// What a name that matches nothing fails with.
pub(crate) const SCHEMA_NAME_NOT_FOUND: &str = "SCHEMA_NAME_NOT_FOUND";

/// `reef schema`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Only one part: `cli`, `mcp` or `api`.
    #[arg(value_enum, value_name = "PART")]
    part: Option<Part>,
    /// One command's words, one tool, or one API service or message.
    #[arg(value_name = "NAME")]
    name: Vec<String>,
    /// Print the API's protobuf descriptor set (binary) instead.
    #[arg(long, conflicts_with_all = ["part", "name"])]
    descriptor_set: bool,
}

/// A part of the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Part {
    // The commands: inputs, `argv`, and output in each form.
    Cli,
    // The MCP tools, with output schemas.
    Mcp,
    // The API's services and messages.
    Api,
}

/// Prints the document, a part of it, or the descriptor set.
pub(crate) fn run(out: &Out, args: &Args) -> Result<(), CliError> {
    if args.descriptor_set {
        return out.raw(reef_api::FILE_DESCRIPTOR_SET);
    }
    let document = crate::schema::document()
        .map_err(|message| internal(format!("could not build the schema: {message}")))?;
    let document = match args.part {
        None => document,
        Some(part) => crate::schema::select(document, part, &args.name)
            .ok_or_else(|| not_found(part, &args.name))?,
    };
    let mut text = match out.mode() {
        Output::Brief => serde_json::to_string(&document),
        Output::Json | Output::Text => serde_json::to_string_pretty(&document),
    }
    .map_err(|error| internal(error.to_string()))?;
    text.push('\n');
    out.raw(text.as_bytes())
}

/// The part's name as the command line spells it.
pub(crate) fn part_name(part: Part) -> &'static str {
    match part {
        Part::Cli => "cli",
        Part::Mcp => "mcp",
        Part::Api => "api",
    }
}

fn not_found(part: Part, name: &[String]) -> CliError {
    let part = part_name(part);
    let hint = match part {
        "cli" => {
            "Name a command by its words, as `reef schema cli ws list`; `reef schema cli` lists them all."
        }
        "mcp" => "Name a tool, as `reef schema mcp get_status`; `reef schema mcp` lists them all.",
        _ => {
            "Name a service or a message, as `reef schema api WorkspaceService`; `reef schema api` lists them all."
        }
    };
    CliError::Precheck {
        kind: ErrorKind::NotFound,
        reason: SCHEMA_NAME_NOT_FOUND,
        message: format!("`{}` names nothing in `reef schema {part}`", name.join(" ")),
        advice: Advice::run(hint, ["reef", "schema", part]),
    }
}

fn internal(message: String) -> CliError {
    CliError::Refused {
        kind: ErrorKind::Internal,
        message,
        advice: None,
    }
}

/// The `cli` part: the global flags once, every leaf command, and the
/// error document every command prints when it fails.
pub(crate) fn part(generator: &mut SchemaGenerator) -> Result<Value, String> {
    let root = commands::root();
    let mut commands = Vec::new();
    for words in super::guide::leaf_commands() {
        let row = outputs::row_of(&words)
            .ok_or_else(|| format!("`{}` has no row in OUTPUTS", words.join(" ")))?;
        let mut entry = leaf_command(&root, &words)?;
        entry["output"] = outputs::describe(row, generator);
        commands.push(entry);
    }
    let kinds = [
        ErrorKind::Internal,
        ErrorKind::InvalidArgument,
        ErrorKind::NotFound,
        ErrorKind::AlreadyExists,
        ErrorKind::Conflict,
        ErrorKind::Unavailable,
        ErrorKind::PermissionDenied,
        ErrorKind::Io,
        ErrorKind::Unsupported,
    ];
    let exit_codes: serde_json::Map<String, Value> = std::iter::once(("ok".to_owned(), json!(0)))
        .chain(kinds.into_iter().map(|kind| {
            (
                super::error::kind_text(kind).to_owned(),
                json!(kind.exit_code()),
            )
        }))
        .collect();
    let document = Value::from(generator.subschema_for::<reef_client::json::ErrorDocument>());
    Ok(json!({
        "argv_rules": "To build a command line: `reef`, the global flags (those with `before_command` first), the command's words, then each input property by its `argv`: a `flag` followed by its value; a boolean's flag alone when true; a count's flag that many times; an array once per element when `repeat`, else its values after the flag; a `position` in order; `after_dashes` last, after `--`. A value that starts with `-` goes as `--flag=value`.",
        "global": global(&root)?,
        "commands": commands,
        "errors": {
            "description": "A command that fails prints this document on stdout when its output is JSON (-o json, -o brief, or a pipe where its `piped` is `brief`), and exits with its kind's code; the text form goes to stderr. A command whose output has `result_on_failure` also exits non-zero with its result.",
            "document": document,
            "exit_codes": exit_codes,
        },
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::outputs::OUTPUTS;
    use super::*;

    fn cli() -> Value {
        let mut generator = SchemaGenerator::new(reef_client::schema::settings());
        part(&mut generator).unwrap()
    }

    fn command<'a>(cli: &'a Value, words: &[&str]) -> &'a Value {
        cli["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["command"] == serde_json::json!(words))
            .unwrap_or_else(|| panic!("no {words:?}"))
    }

    #[test]
    fn every_leaf_command_has_one_row() {
        let cli = cli();
        let leaves = super::super::guide::leaf_commands();
        let listed: Vec<Vec<String>> = cli["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| serde_json::from_value(entry["command"].clone()).unwrap())
            .collect();
        assert_eq!(listed, leaves);
        for leaf in &leaves {
            let rows = OUTPUTS
                .iter()
                .filter(|row| row.command.iter().eq(leaf.iter()))
                .count();
            assert_eq!(rows, 1, "{leaf:?} has {rows} rows");
        }
        for row in OUTPUTS {
            assert!(
                leaves.iter().any(|leaf| row.command.iter().eq(leaf.iter())),
                "a row for no command: {:?}",
                row.command
            );
            if row.json.is_none() {
                assert!(
                    row.note.is_some(),
                    "{:?} prints no JSON and says nothing",
                    row.command
                );
            }
        }
    }

    #[test]
    fn inputs_describe_clap() {
        let root = commands::root();
        let cli = cli();
        for words in super::super::guide::leaf_commands() {
            let mut clap = &root;
            for word in &words {
                clap = clap.find_subcommand(word).unwrap();
            }
            let entry = command(&cli, &words.iter().map(String::as_str).collect::<Vec<_>>());
            let properties = entry["input"]["properties"].as_object().unwrap();
            let argv = entry["argv"].as_object().unwrap();
            let visible: Vec<&str> = clap
                .get_arguments()
                .filter(|arg| {
                    !arg.is_hide_set()
                        && !arg.is_global_set()
                        && !matches!(
                            arg.get_action(),
                            clap::ArgAction::Help
                                | clap::ArgAction::HelpShort
                                | clap::ArgAction::HelpLong
                                | clap::ArgAction::Version
                        )
                })
                .map(|arg| arg.get_id().as_str())
                .collect();
            let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
            let mut expected = visible.clone();
            keys.sort_unstable();
            expected.sort_unstable();
            assert_eq!(keys, expected, "{words:?}");
            for id in &visible {
                let arg = clap.get_arguments().find(|arg| arg.get_id() == id).unwrap();
                let words_of = &argv[*id];
                if arg.is_positional() {
                    assert!(
                        words_of.get("position").is_some() || words_of["after_dashes"] == true,
                        "{words:?} {id}: {words_of}"
                    );
                } else {
                    assert_eq!(
                        words_of["flag"],
                        format!("--{}", arg.get_long().unwrap()),
                        "{words:?} {id}"
                    );
                }
            }
        }
        let limit = &command(&cli, &["ws", "list"])["input"]["properties"]["limit"];
        assert_eq!(limit["type"], "integer", "{limit}");
        assert_eq!(limit["default"], 100, "{limit}");
        let output = &cli["global"]["input"]["properties"]["output"];
        assert_eq!(output["enum"], serde_json::json!(["text", "json", "brief"]));
        assert_eq!(cli["global"]["argv"]["output"]["short"], "-o");
        let run = command(&cli, &["ws", "run"]);
        assert_eq!(run["argv"]["command"]["after_dashes"], true);
        assert_eq!(run["input"]["properties"]["command"]["type"], "array");
        assert_eq!(run["input"]["required"], serde_json::json!(["command"]));
        assert_eq!(run["argv"]["env"]["repeat"], true);
        let clear = command(&cli, &["presence", "clear"]);
        assert!(clear["input"]["allOf"][0]["anyOf"].is_array(), "{clear}");
        let ls = command(&cli, &["ws", "list"]);
        assert!(
            ls["aliases"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(["workspace", "ls"])),
            "{ls}"
        );
    }

    /// Words that pass `arg` with a value clap accepts.
    fn sample(arg: &clap::Arg) -> Vec<String> {
        let value = || {
            if let Some(first) = arg.get_possible_values().first() {
                return first.get_name().to_owned();
            }
            let id = arg.get_value_parser().type_id();
            if id == std::any::TypeId::of::<std::time::Duration>() {
                "1s".to_owned()
            } else if id == std::any::TypeId::of::<std::time::SystemTime>() {
                "1h".to_owned()
            } else if id == std::any::TypeId::of::<(String, String)>() {
                "K=V".to_owned()
            } else if [
                std::any::TypeId::of::<u8>(),
                std::any::TypeId::of::<u16>(),
                std::any::TypeId::of::<u32>(),
                std::any::TypeId::of::<u64>(),
                std::any::TypeId::of::<usize>(),
                std::any::TypeId::of::<i32>(),
                std::any::TypeId::of::<i64>(),
            ]
            .into_iter()
            .any(|other| id == other)
            {
                "1".to_owned()
            } else {
                "x".to_owned()
            }
        };
        if arg.is_positional() {
            return vec![value()];
        }
        let flag = format!("--{}", arg.get_long().unwrap());
        match arg.get_action() {
            clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Count => {
                vec![flag]
            }
            _ => vec![format!("{flag}={}", value())],
        }
    }

    /// How clap takes `words` with `given` (argument ids) and what the
    /// command needs anyway: `Ok`, or the error's kind.
    fn parse(
        root: &clap::Command,
        command: &clap::Command,
        words: &[String],
        given: &[&str],
    ) -> Result<(), clap::error::ErrorKind> {
        let arg = |id: &str| {
            command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap()
        };
        let mut ids: Vec<&str> = given.to_vec();
        for arg in command.get_arguments() {
            if arg.is_required_set() && !ids.contains(&arg.get_id().as_str()) {
                ids.push(arg.get_id().as_str());
            }
        }
        // A positional comes after every earlier one.
        let given_positions: Vec<usize> = ids.iter().filter_map(|id| arg(id).get_index()).collect();
        if let Some(last) = given_positions.into_iter().max() {
            for earlier in command.get_positionals() {
                if earlier.get_index().is_some_and(|index| index < last)
                    && !ids.contains(&earlier.get_id().as_str())
                {
                    ids.push(earlier.get_id().as_str());
                }
            }
        }
        for group in command.get_groups().filter(|group| group.is_required_set()) {
            let members: Vec<&str> = group.get_args().map(clap::Id::as_str).collect();
            if !members.iter().any(|member| ids.contains(member)) {
                ids.push(members[0]);
            }
        }
        let mut argv: Vec<String> = vec!["reef".to_owned()];
        argv.extend(words.iter().cloned());
        let mut positionals: Vec<&clap::Arg> = Vec::new();
        let mut trailing = Vec::new();
        for id in &ids {
            let arg = arg(id);
            if arg.is_trailing_var_arg_set() || arg.is_last_set() {
                trailing = vec!["--".to_owned(), "true".to_owned()];
            } else if arg.is_positional() {
                positionals.push(arg);
            } else {
                argv.extend(sample(arg));
            }
        }
        positionals.sort_by_key(|arg| arg.get_index());
        for arg in positionals {
            argv.extend(sample(arg));
        }
        argv.extend(trailing);
        root.clone()
            .try_get_matches_from(&argv)
            .map(|_| ())
            .map_err(|error| error.kind())
    }

    #[test]
    fn constraints_match_clap() {
        use clap::error::ErrorKind as Clap;
        let root = commands::root();
        let cli = cli();
        for words in super::super::guide::leaf_commands() {
            let mut clap = &root;
            for word in &words {
                clap = clap.find_subcommand(word).unwrap();
            }
            let words_of: Vec<&str> = words.iter().map(String::as_str).collect();
            let entry = command(&cli, &words_of);
            let ids: Vec<&str> = clap
                .get_arguments()
                .filter(|arg| !arg.is_hide_set() && !arg.is_global_set() && !commands::is_meta(arg))
                .map(|arg| arg.get_id().as_str())
                .collect();
            assert_eq!(parse(&root, clap, &words, &[]), Ok(()), "{words:?} alone");
            let conflicts = commands::conflicts(clap);
            for (index, a) in ids.iter().enumerate() {
                // Alone: accepted, or refused for what it needs, as listed.
                let needs = commands::requires(&words_of, a);
                let alone = parse(&root, clap, &words, &[a]);
                if needs.is_empty() {
                    assert_eq!(alone, Ok(()), "{words:?} --{a}");
                } else {
                    assert_eq!(alone, Err(Clap::MissingRequiredArgument), "{words:?} --{a}");
                    for need in needs {
                        assert_eq!(
                            parse(&root, clap, &words, &[a, need]),
                            Ok(()),
                            "{words:?} {a} {need}"
                        );
                    }
                    assert_eq!(
                        entry["argv"][*a]["requires_one_of"],
                        serde_json::json!(needs)
                    );
                }
                // Two together: refused exactly when listed.
                for b in &ids[index + 1..] {
                    let mut both = vec![*a, *b];
                    for id in [*a, *b] {
                        let needs = commands::requires(&words_of, id);
                        if let Some(need) = needs.first()
                            && !needs.iter().any(|need| both.contains(need))
                        {
                            both.push(need);
                        }
                    }
                    let listed = conflicts
                        .iter()
                        .any(|(x, y)| (x == a && y == b) || (x == b && y == a));
                    let refused = parse(&root, clap, &words, &both) == Err(Clap::ArgumentConflict);
                    assert_eq!(refused, listed, "{words:?} --{a} --{b}");
                    if listed {
                        let against = entry["argv"][*a]["conflicts_with"].as_array().unwrap();
                        assert!(against.contains(&serde_json::json!(b)), "{words:?} {a} {b}");
                        let against = entry["argv"][*b]["conflicts_with"].as_array().unwrap();
                        assert!(against.contains(&serde_json::json!(a)), "{words:?} {b} {a}");
                    }
                }
            }
        }
        // REQUIRES names only real commands and arguments.
        let walked = super::super::guide::leaf_commands();
        for (command, arg, _) in commands::REQUIRES {
            assert!(
                walked.iter().any(|w| w.iter().eq(command.iter())),
                "{command:?}"
            );
            assert!(!command.is_empty() && !arg.is_empty());
        }
        assert_eq!(cli["global"]["argv"]["remote"]["before_command"], true);
        assert!(
            cli["global"]["argv"]["output"]
                .get("before_command")
                .is_none()
        );
    }
}
