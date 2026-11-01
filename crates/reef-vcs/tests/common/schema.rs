//! Output checked against `reef schema` (R2.24): [`check`] validates what
//! a `reef` command line printed against its command's schema, in the
//! form it printed, and every failure against the error document;
//! [`check_tool`] validates an MCP tool's result against the tool's
//! `outputSchema`.
//!
//! nextest runs each test in its own process, so the document is cached
//! in `CARGO_TARGET_TMPDIR`, keyed by the `reef` binary's size and mtime,
//! and `reef schema` runs only on a miss.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

/// `reef schema -o json` of the `reef` under test.
pub(crate) fn document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(|| {
        let reef = Path::new(env!("CARGO_BIN_EXE_reef"));
        let meta = std::fs::metadata(reef).unwrap();
        let modified = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cache = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("reef-schema-{}-{modified}.json", meta.len()));
        if let Ok(text) = std::fs::read_to_string(&cache)
            && let Ok(document) = serde_json::from_str(&text)
        {
            return document;
        }
        let output = Command::new(reef)
            .args(["schema", "-o", "json"])
            .env_remove("REEF_OUTPUT")
            .output()
            .unwrap();
        assert!(output.status.success(), "reef schema failed: {output:?}");
        // Another test process may write the same file: write a copy and
        // rename it into place.
        let partial = cache.with_extension(format!("{}.partial", std::process::id()));
        std::fs::write(&partial, &output.stdout).unwrap();
        std::fs::rename(&partial, &cache).unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    })
}

/// A compiled validator per schema, by its JSON text.
fn validator(schema: &Value) -> Arc<jsonschema::Validator> {
    static VALIDATORS: OnceLock<Mutex<HashMap<String, Arc<jsonschema::Validator>>>> =
        OnceLock::new();
    let key = schema.to_string();
    let validators = VALIDATORS.get_or_init(Mutex::default);
    if let Some(validator) = validators.lock().unwrap().get(&key) {
        return Arc::clone(validator);
    }
    let compiled = Arc::new(
        jsonschema::draft202012::options()
            .should_validate_formats(false)
            .build(schema)
            .unwrap_or_else(|error| panic!("the schema does not compile: {error}: {schema}")),
    );
    validators
        .lock()
        .unwrap()
        .insert(key, Arc::clone(&compiled));
    compiled
}

/// Whether `instance` matches `shape`, a schema whose references point
/// into the document's `$defs`; the errors otherwise.
pub(crate) fn validate(shape: &Value, instance: &Value) -> Result<(), String> {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": document()["$defs"],
        "allOf": [shape],
    });
    validate_with(&schema, instance)
}

/// Whether `instance` matches the self-contained `schema`.
pub(crate) fn validate_with(schema: &Value, instance: &Value) -> Result<(), String> {
    let validator = validator(schema);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// The document's entry for the command `args` runs (`reef` and global
/// flags not included in `args`' words): the longest run of words that
/// names a command, by name, visible alias or unique prefix.
pub(crate) fn command_of(args: &[&str]) -> Option<&'static Value> {
    let document = document();
    let commands = document["cli"]["commands"].as_array()?;
    let takes_value: Vec<String> = document["cli"]["global"]["input"]["properties"]
        .as_object()?
        .iter()
        .filter(|(_, schema)| !matches!(schema["type"].as_str(), Some("boolean" | "integer")))
        .filter_map(|(id, _)| {
            document["cli"]["global"]["argv"][id]["flag"]
                .as_str()
                .map(str::to_owned)
        })
        .chain(["-o".to_owned()])
        .collect();
    // Every word that is not a flag or a global flag's value, up to `--`:
    // a command's own values come after its words, and the longest
    // spelling that matches wins.
    let mut words = Vec::new();
    let mut iter = args.iter().take_while(|arg| **arg != "--");
    while let Some(arg) = iter.next() {
        if arg.starts_with('-') {
            if takes_value.iter().any(|flag| flag == arg) {
                iter.next();
            }
            continue;
        }
        words.push(*arg);
    }
    // Spellings of every command, by its words.
    let spellings = |entry: &'static Value| -> Vec<Vec<String>> {
        std::iter::once(&entry["command"])
            .chain(entry["aliases"].as_array().into_iter().flatten())
            .filter_map(|words| serde_json::from_value(words.clone()).ok())
            .collect()
    };
    for len in (1..=words.len()).rev() {
        let given = &words[..len];
        let matches: Vec<&'static Value> = commands
            .iter()
            .filter(|entry| {
                spellings(entry).iter().any(|spelling| {
                    spelling.len() == len
                        && spelling
                            .iter()
                            .zip(given)
                            .all(|(name, word)| name.starts_with(word))
                })
            })
            .collect();
        if let [entry] = matches.as_slice() {
            return Some(entry);
        }
        if let Some(exact) = matches.iter().find(|entry| {
            spellings(entry).iter().any(|spelling| {
                spelling
                    .iter()
                    .map(String::as_str)
                    .eq(given.iter().copied())
            })
        }) {
            return Some(exact);
        }
    }
    None
}

/// The form `args` asks for: `json`, `brief` or `text`, or `None` for the
/// command's piped default.
fn form(args: &[&str]) -> Option<&'static str> {
    let mut found = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if *arg == "--" {
            break;
        }
        let value = match *arg {
            "-o" | "--output" => iter.next().copied(),
            _ => arg
                .strip_prefix("--output=")
                .or_else(|| arg.strip_prefix("-o")),
        };
        found = match value {
            Some("json") => Some("json"),
            Some("brief") => Some("brief"),
            Some("text" | "toml") => Some("text"),
            _ => found,
        };
    }
    found
}

/// Checks what `reef <args>` printed on stdout against its command's
/// schema in the form it printed: a success's output, or a failure's
/// error document. Text output is not checked. Panics with the
/// differences.
pub(crate) fn check(args: &[&str], output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let words = args.iter().take_while(|arg| **arg != "--");
    if words
        .clone()
        .any(|arg| matches!(*arg, "--help" | "--help-all" | "-h" | "--version" | "-V"))
    {
        return;
    }
    let error_document = &document()["cli"]["errors"]["document"];
    let Some(entry) = command_of(args) else {
        // `reef` alone and clap's `help` print text; anything else that
        // names no command is a usage error, whose JSON is the error.
        let mut named = words.filter(|arg| !arg.starts_with('-'));
        let first = named.next();
        assert!(
            !output.status.success() || first.is_none_or(|word| word == &"help"),
            "reef {args:?} succeeded but names no command in reef schema"
        );
        if !output.status.success()
            && let Ok(error) = serde_json::from_str::<Value>(&stdout)
        {
            report(args, error_document, &error);
        }
        return;
    };
    let output_entry = &entry["output"];
    let given = |ids: &Value| {
        ids.as_array().into_iter().flatten().any(|id| {
            let flag = &entry["argv"][id.as_str().unwrap_or_default()]["flag"];
            words.clone().any(|arg| flag == *arg)
        })
    };
    if given(&output_entry["bytes_with"]) {
        return;
    }
    // Without `-o`, `REEF_OUTPUT` may choose, which `args` do not show:
    // the full form prints indented, the brief one compact. A command
    // whose pipe gets text is not checked then: its stdout may be another
    // program's JSON (`ws run -- reef here -o json`).
    let form = match form(args) {
        Some(form) => form,
        None if output_entry["piped"] != "brief" || given(&output_entry["text_with"]) => "text",
        None if stdout.contains("\n  ") => "json",
        None => "brief",
    };
    if form == "text" || output.status.code().is_none() {
        // Text, or killed by a signal mid-output.
        return;
    }
    let shape = &output_entry[form];
    if shape.is_null() {
        return;
    }
    let lines = output_entry["lines"] == true;
    if !output.status.success() && output_entry["result_on_failure"] != true {
        // The error document; a stream's earlier lines are its values.
        let mut values: Vec<&str> = if lines {
            stdout.lines().filter(|line| !line.is_empty()).collect()
        } else {
            vec![stdout.as_ref()]
        };
        let error = values.pop().unwrap_or_default();
        for value in values {
            report(args, shape, &parse(args, value));
        }
        report(args, error_document, &parse(args, error));
        return;
    }
    let shape = if output.status.success() {
        shape.clone()
    } else {
        json!({ "anyOf": [shape, error_document] })
    };
    let values: Vec<Value> = if lines {
        stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| parse(args, line))
            .collect()
    } else if output_entry["last_value"] == true {
        vec![last_value(args, &stdout)]
    } else {
        vec![parse(args, &stdout)]
    };
    for value in &values {
        report(args, &shape, value);
    }
}

/// The JSON value that ends `stdout`, after another program's output: the
/// longest tail from a line's start that parses.
fn last_value(args: &[&str], stdout: &str) -> Value {
    let mut starts = std::iter::once(0).chain(stdout.match_indices('\n').map(|(at, _)| at + 1));
    starts
        .find_map(|start| serde_json::from_str(&stdout[start..]).ok())
        .unwrap_or_else(|| panic!("reef {args:?} printed no JSON at the end: {stdout}"))
}

fn parse(args: &[&str], text: &str) -> Value {
    serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("reef {args:?} printed no JSON ({error}): {text}"))
}

fn report(args: &[&str], shape: &Value, value: &Value) {
    if let Err(errors) = validate(shape, value) {
        panic!("reef {args:?}'s output does not match its schema: {errors}\noutput: {value}");
    }
}

/// `reef schema mcp`'s definition of the tool `name`.
pub(crate) fn tool(name: &str) -> &'static Value {
    document()["mcp"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
        .unwrap_or_else(|| panic!("no tool {name} in reef schema mcp"))
}

/// Checks a tool's structured content against its `outputSchema`, or the
/// error result's schema when `is_error`.
pub(crate) fn check_tool(name: &str, is_error: bool, value: &Value) {
    let schema = if is_error {
        &document()["mcp"]["error_result"]
    } else {
        &tool(name)["outputSchema"]
    };
    if let Err(errors) = validate_with(schema, value) {
        panic!("{name}'s result does not match its schema: {errors}\nresult: {value}");
    }
}
