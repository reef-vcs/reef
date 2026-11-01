//! Each command's inputs from clap's tree: a plain JSON Schema object a
//! harness can use as a tool's input schema unchanged, keyed by clap's
//! argument ids, and beside it `argv`, how to turn such an object back
//! into words. A value type not classified here is an error, so a new one
//! is classified on purpose (test `inputs_describe_clap`).

use std::any::TypeId;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use clap::{Arg, ArgAction};
use serde_json::{Map, Value, json};

/// The command tree, built, so every argument's action and value count
/// are set.
pub(crate) fn root() -> clap::Command {
    let mut root = crate::cli::command_all();
    root.build();
    root
}

/// The leaf command at `words`, as `{command, aliases, about, input,
/// argv}`.
pub(crate) fn leaf_command(root: &clap::Command, words: &[String]) -> Result<Value, String> {
    let mut command = root;
    let mut spellings: Vec<Vec<String>> = vec![Vec::new()];
    for word in words {
        command = command
            .find_subcommand(word)
            .ok_or_else(|| format!("no command `{}`", words.join(" ")))?;
        let names: Vec<&str> = std::iter::once(command.get_name())
            .chain(command.get_visible_aliases())
            .collect();
        spellings = spellings
            .iter()
            .flat_map(|prefix| {
                names.iter().map(move |name| {
                    let mut spelling = prefix.clone();
                    spelling.push((*name).to_owned());
                    spelling
                })
            })
            .collect();
    }
    // The first spelling is every word's own name.
    let aliases: Vec<Vec<String>> = spellings.into_iter().skip(1).collect();
    let words_of: Vec<&str> = words.iter().map(String::as_str).collect();
    let (input, argv) = inputs(
        command,
        &words_of,
        |arg| !arg.is_global_set(),
        &words.join(" "),
    )?;
    Ok(json!({
        "command": words,
        "aliases": aliases,
        "about": command.get_about().map(ToString::to_string),
        "input": input,
        "argv": argv,
    }))
}

/// The flags every command takes, and `--remote`, which goes before the
/// command: `{description, input, argv}`.
pub(crate) fn global(root: &clap::Command) -> Result<Value, String> {
    let (input, mut argv) = inputs(root, &[], |_| true, "reef")?;
    for arg in root.get_arguments() {
        if !arg.is_global_set()
            && let Some(Value::Object(words)) = argv.get_mut(arg.get_id().as_str())
        {
            words.insert("before_command".to_owned(), json!(true));
        }
    }
    Ok(json!({
        "description": "Flags every command takes, before or after its words; one with `before_command` goes before them.",
        "input": input,
        "argv": argv,
    }))
}

/// Arguments that need another before clap accepts them (`requires`),
/// by command words and argument id: any one of the listed ids. clap keeps
/// `requires` to itself, so they are listed here; test
/// `constraints_match_clap` probes clap for every argument and fails until
/// this list is complete.
pub(crate) const REQUIRES: &[(&[&str], &str, &[&str])] = &[
    (&["diff"], "call_sites", &["symbols", "expand"]),
    (&["diff"], "source", &["symbols", "expand"]),
    (&["history"], "change_kind", &["symbol"]),
];

/// What `reef schema` says `id` of the command at `words` needs: any one
/// of these.
pub(crate) fn requires(words: &[&str], id: &str) -> &'static [&'static str] {
    REQUIRES
        .iter()
        .find(|(command, arg, _)| *command == words && *arg == id)
        .map_or(&[], |(_, _, any)| any)
}

/// Pairs of `command`'s arguments that clap refuses together: each one's
/// `conflicts_with`, both ways, and two members of a group that is not
/// `multiple`. Sorted, each pair in order.
pub(crate) fn conflicts(command: &clap::Command) -> Vec<(String, String)> {
    let mut pairs = std::collections::BTreeSet::new();
    let mut add = |a: &str, b: &str| {
        if a != b {
            let (a, b) = if a < b { (a, b) } else { (b, a) };
            pairs.insert((a.to_owned(), b.to_owned()));
        }
    };
    for arg in command.get_arguments() {
        for other in command.get_arg_conflicts_with(arg) {
            add(arg.get_id().as_str(), other.get_id().as_str());
        }
    }
    for group in command.get_groups() {
        if group.clone().is_multiple() {
            continue;
        }
        let members: Vec<&str> = group.get_args().map(clap::Id::as_str).collect();
        for (index, first) in members.iter().enumerate() {
            for second in &members[index + 1..] {
                add(first, second);
            }
        }
    }
    pairs.into_iter().collect()
}

/// `command`'s visible arguments that `keep` keeps, as an input schema and
/// its `argv`. The schema carries what clap enforces beyond each value:
/// `required`, a required group as `anyOf`, each pair clap refuses
/// together as `not: {required: [a, b]}`, and what an argument needs as
/// `dependentSchemas`.
fn inputs(
    command: &clap::Command,
    words_of: &[&str],
    keep: impl Fn(&Arg) -> bool,
    name: &str,
) -> Result<(Value, Value), String> {
    let mut properties = Map::new();
    let mut argv = Map::new();
    let mut required = Vec::new();
    let mut position = 0;
    let conflicts = conflicts(command);
    let mut kept = Vec::new();
    for arg in command.get_arguments() {
        if arg.is_hide_set() || !keep(arg) || is_meta(arg) {
            continue;
        }
        let id = arg.get_id().as_str().to_owned();
        let schema = schema(arg).map_err(|kind| {
            format!("`{name}`'s argument `{id}` has a value type `reef schema` does not classify: {kind}")
        })?;
        properties.insert(id.clone(), schema);
        let mut words = words(arg, &mut position);
        let against: Vec<&str> = conflicts
            .iter()
            .filter_map(|(a, b)| match (a == &id, b == &id) {
                (true, _) => Some(b.as_str()),
                (_, true) => Some(a.as_str()),
                _ => None,
            })
            .collect();
        if !against.is_empty() {
            words["conflicts_with"] = json!(against);
        }
        let needs = requires(words_of, &id);
        if !needs.is_empty() {
            words["requires_one_of"] = json!(needs);
        }
        argv.insert(id.clone(), words);
        if arg.is_required_set() {
            required.push(id.clone());
        }
        kept.push(id);
    }
    let mut input = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        input["required"] = json!(required);
    }
    let mut all_of: Vec<Value> = command
        .get_groups()
        .filter(|group| group.is_required_set())
        .map(|group| {
            let any_of: Vec<Value> = group
                .get_args()
                .map(|id| json!({ "required": [id.as_str()] }))
                .collect();
            json!({ "anyOf": any_of })
        })
        .collect();
    for (a, b) in &conflicts {
        if kept.contains(a) && kept.contains(b) {
            all_of.push(json!({ "not": { "required": [a, b] } }));
        }
    }
    if !all_of.is_empty() {
        input["allOf"] = Value::Array(all_of);
    }
    let dependent: Map<String, Value> = kept
        .iter()
        .filter_map(|id| {
            let needs = requires(words_of, id);
            (!needs.is_empty()).then(|| {
                let any_of: Vec<Value> = needs
                    .iter()
                    .map(|need| json!({ "required": [need] }))
                    .collect();
                (id.clone(), json!({ "anyOf": any_of }))
            })
        })
        .collect();
    if !dependent.is_empty() {
        input["dependentSchemas"] = Value::Object(dependent);
    }
    Ok((input, Value::Object(argv)))
}

/// `--help` and `--version`, which every command has and no harness
/// passes.
pub(crate) fn is_meta(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

/// How the argument is passed: `{flag, short?}` or `{position}`, or
/// `{after_dashes}` for a trailing command; `repeat` when an array is
/// passed as the flag once per element. A flag of `reef` itself that is
/// not global goes before the command's words (`before_command`).
fn words(arg: &Arg, position: &mut usize) -> Value {
    let mut words = Map::new();
    if arg.is_positional() {
        if arg.is_trailing_var_arg_set() || arg.is_last_set() {
            words.insert("after_dashes".to_owned(), json!(true));
        } else {
            words.insert("position".to_owned(), json!(*position));
            *position += 1;
        }
    } else {
        if let Some(long) = arg.get_long() {
            words.insert("flag".to_owned(), json!(format!("--{long}")));
        }
        if let Some(short) = arg.get_short() {
            words.insert("short".to_owned(), json!(format!("-{short}")));
        }
        if matches!(arg.get_action(), ArgAction::Append) {
            words.insert("repeat".to_owned(), json!(true));
        }
    }
    Value::Object(words)
}

/// The argument's value as JSON Schema, with its help and default; `Err`
/// names a value type not classified here.
fn schema(arg: &Arg) -> Result<Value, String> {
    let mut schema = match arg.get_action() {
        ArgAction::SetTrue | ArgAction::SetFalse => json!({ "type": "boolean" }),
        ArgAction::Count => json!({ "type": "integer", "minimum": 0 }),
        _ => value(arg)?,
    };
    let multiple = matches!(arg.get_action(), ArgAction::Append)
        || arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1);
    // A flag's `false` and a count's 0 go without saying.
    let defaults: Vec<Value> = match arg.get_action() {
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count => Vec::new(),
        _ => arg
            .get_default_values()
            .iter()
            .map(|text| typed(&schema, &text.to_string_lossy()))
            .collect(),
    };
    if multiple {
        schema = json!({ "type": "array", "items": schema });
    }
    let object = schema.as_object_mut().ok_or("not an object")?;
    if let Some(help) = arg.get_help().or_else(|| arg.get_long_help()) {
        // The value's own note (`a duration such as ...`) follows the help.
        let help = help.to_string();
        let help = match object.get("description").and_then(Value::as_str) {
            Some(note)
                if !help.contains("RFC 3339") && !help.to_lowercase().contains("duration") =>
            {
                format!("{help} ({note})")
            }
            _ => help,
        };
        object.insert("description".to_owned(), json!(help));
    }
    match (multiple, defaults.as_slice()) {
        (_, []) => {}
        (false, [only]) => {
            object.insert("default".to_owned(), only.clone());
        }
        (_, many) => {
            object.insert("default".to_owned(), Value::Array(many.to_vec()));
        }
    }
    Ok(schema)
}

/// The schema of one value of a `Set` or `Append` argument.
fn value(arg: &Arg) -> Result<Value, String> {
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_owned())
        .collect();
    if !possible.is_empty() {
        return Ok(json!({ "type": "string", "enum": possible }));
    }
    let id = arg.get_value_parser().type_id();
    let is = |other: TypeId| id == other;
    Ok(
        if [
            TypeId::of::<String>(),
            TypeId::of::<PathBuf>(),
            TypeId::of::<OsString>(),
        ]
        .into_iter()
        .any(is)
        {
            json!({ "type": "string" })
        } else if [
            TypeId::of::<u8>(),
            TypeId::of::<u16>(),
            TypeId::of::<u32>(),
            TypeId::of::<u64>(),
            TypeId::of::<usize>(),
        ]
        .into_iter()
        .any(is)
        {
            json!({ "type": "integer", "minimum": 0 })
        } else if [TypeId::of::<i32>(), TypeId::of::<i64>()]
            .into_iter()
            .any(is)
        {
            json!({ "type": "integer" })
        } else if is(TypeId::of::<Duration>()) {
            json!({ "type": "string", "description": "a duration such as `30s` or `2h`" })
        } else if is(TypeId::of::<SystemTime>()) {
            json!({ "type": "string", "description": "an RFC 3339 time, or a duration ago such as `7d`" })
        } else if is(TypeId::of::<(String, String)>()) {
            json!({ "type": "string", "pattern": "^[^=]+=" })
        } else {
            return Err(format!("{id:?}"));
        },
    )
}

/// A default as the value `schema` describes: a number for an integer.
fn typed(schema: &Value, text: &str) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("integer") => text
            .parse::<i64>()
            .map_or_else(|_| json!(text), Value::from),
        Some("boolean") => text
            .parse::<bool>()
            .map_or_else(|_| json!(text), Value::from),
        _ => json!(text),
    }
}
