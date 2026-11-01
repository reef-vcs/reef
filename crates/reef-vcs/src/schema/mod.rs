//! The document `reef schema` prints (R2.24, ADR 0050): one JSON object
//! that is also a JSON Schema 2020-12 resource. Its `cli`, `mcp` and `api`
//! members are annotations, and every schema they reference lives under
//! `$defs`, so `urn:reef-vcs:schema:<version>#/$defs/Inspected` resolves
//! against it.
//!
//! `$defs` holds `reef_client::json`'s types by their Rust name, the brief
//! views as `brief.<Name>`, the CLI's own types as `cli.<Name>`, the MCP
//! results as `mcp.<Name>`, and the API's messages and enums by full name
//! (`reef.api.v1.Workspace`). One generator builds the Rust types, so each
//! is defined once. Keys print sorted and lists in a fixed order, so two
//! runs print the same bytes.

pub(crate) mod api;

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::cli::schema::Part;

/// The document's layout version: changes only when a member is renamed or
/// moved, not when one is added.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The whole document.
pub(crate) fn document() -> Result<Value, String> {
    let mut generator = schemars::SchemaGenerator::new(reef_client::schema::settings());
    let cli = crate::cli::schema::part(&mut generator)?;
    let mut defs = generator.take_definitions(true);
    reef_client::schema::describe_brief(&mut defs);
    let mcp = crate::mcp::schema_part();
    let pool = api::pool().ok_or("the API's descriptor set does not decode")?;
    let api = api::part(&pool, &mut defs);
    let version = env!("CARGO_PKG_VERSION");
    let mut document = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("urn:reef-vcs:schema:{version}"),
        "description": "Reef's commands, MCP tools and API as JSON Schema (`reef schema`, ADR 0050). Each `cli.commands[]` entry gives an input schema, the `argv` that turns an input into words, and the output with -o json, -o brief and on a pipe; `mcp.tools` are the MCP tools with output schemas; `api.services` are the gRPC services, whose messages are described as canonical proto3 JSON.",
        "reef": {
            "version": version,
            "commit": env!("REEF_BUILD_COMMIT"),
            "target": env!("REEF_BUILD_TARGET"),
            "schema_version": SCHEMA_VERSION,
            "api_package": api::PACKAGE,
            "api_revision": reef_api::API_REVISION,
            "api_digest": reef_api::API_DIGEST,
        },
        "cli": cli,
        "mcp": mcp,
        "api": api,
        "$defs": defs,
    });
    reef_client::schema::strip_doc_links(&mut document);
    Ok(document)
}

/// The document with only `part`, or one name in it, and the definitions
/// that reaches; `None` when `name` matches nothing.
pub(crate) fn select(mut document: Value, part: Part, name: &[String]) -> Option<Value> {
    let object = document.as_object_mut()?;
    for other in ["cli", "mcp", "api"] {
        if other != crate::cli::schema::part_name(part) {
            object.remove(other);
        }
    }
    if !name.is_empty() {
        match part {
            Part::Cli => keep_command(object.get_mut("cli")?, name)?,
            Part::Mcp => keep_tool(object.get_mut("mcp")?, name)?,
            Part::Api => keep_api(object, name)?,
        }
    }
    prune(object);
    Some(document)
}

/// Keeps the command `words` names, resolved by clap itself (names,
/// aliases, unique prefixes where the parent infers them), or every
/// command of the group it names (`ws`).
fn keep_command(cli: &mut Value, words: &[String]) -> Option<()> {
    let path = resolve(words)?;
    let commands = cli.get_mut("commands")?.as_array_mut()?;
    commands.retain(|entry| {
        entry
            .get("command")
            .and_then(Value::as_array)
            .is_some_and(|words| words.starts_with(&path))
    });
    (!commands.is_empty()).then_some(())
}

/// The command or group `words` name, as clap parses them: each word one
/// subcommand, by its own name. `None` when a word is not one.
fn resolve(words: &[String]) -> Option<Vec<Value>> {
    if words.iter().any(|word| word.starts_with('-')) {
        return None;
    }
    let matches = crate::cli::schema::commands_root()
        .ignore_errors(true)
        .try_get_matches_from(std::iter::once("reef").chain(words.iter().map(String::as_str)))
        .ok()?;
    let mut path = Vec::new();
    let mut current = &matches;
    while let Some((name, sub)) = current.subcommand() {
        path.push(json!(name));
        current = sub;
    }
    (path.len() == words.len() && path.last() != Some(&json!("help"))).then_some(path)
}

fn keep_tool(mcp: &mut Value, name: &[String]) -> Option<()> {
    let [name] = name else {
        return None;
    };
    let tools = mcp.get_mut("tools")?.as_array_mut()?;
    tools.retain(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()));
    (!tools.is_empty()).then_some(())
}

/// Keeps one service (with its methods' messages), or one message or
/// enum of the package, by full name or by its name in the package
/// (`OutputLine.Stream` for a nested one).
fn keep_api(document: &mut Map<String, Value>, name: &[String]) -> Option<()> {
    let [name] = name else {
        return None;
    };
    let prefix = format!("{}.", api::PACKAGE);
    let full = if name.starts_with(&prefix) {
        name.clone()
    } else {
        format!("{prefix}{name}")
    };
    let services = document
        .get_mut("api")?
        .get_mut("services")?
        .as_object_mut()?;
    if services.contains_key(&full) {
        services.retain(|key, _| *key == full);
        return Some(());
    }
    services.clear();
    let defs = document.get("$defs")?.as_object()?;
    if !defs.contains_key(&full) {
        return None;
    }
    document
        .get_mut("api")?
        .as_object_mut()?
        .insert("schema".to_owned(), api::reference(&full));
    Some(())
}

/// Drops every definition that nothing kept references, directly or
/// through another definition.
fn prune(document: &mut Map<String, Value>) {
    let Some(Value::Object(defs)) = document.remove("$defs") else {
        return;
    };
    let mut reached = BTreeSet::new();
    let mut pending = Vec::new();
    refs(&Value::Object(document.clone()), &mut pending);
    while let Some(name) = pending.pop() {
        if reached.insert(name.clone())
            && let Some(def) = defs.get(&name)
        {
            refs(def, &mut pending);
        }
    }
    let kept: Map<String, Value> = defs
        .into_iter()
        .filter(|(name, _)| reached.contains(name))
        .collect();
    document.insert("$defs".to_owned(), Value::Object(kept));
}

/// The definitions `value` references.
fn refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                match (key.as_str(), value) {
                    ("$ref", Value::String(target)) => {
                        if let Some(name) = target.strip_prefix("#/$defs/") {
                            out.push(name.to_owned());
                        }
                    }
                    // An MCP tool's schemas are self-contained.
                    ("outputSchema" | "inputSchema" | "error_result", _) => {}
                    _ => refs(value, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| refs(item, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use prost_reflect::{
        DynamicMessage, FieldDescriptor, Kind, MapKey, MessageDescriptor, SerializeOptions,
        Value as Reflected,
    };

    use super::*;

    /// A validator of `name`'s definition in `document`.
    fn validator(document: &Value, name: &str) -> jsonschema::Validator {
        let mut schema = document.clone();
        schema["$ref"] = api::reference(name)["$ref"].clone();
        jsonschema::draft202012::options()
            .should_validate_formats(false)
            .build(&schema)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
    }

    #[test]
    fn api_covers_the_descriptor_set() {
        let document = document().unwrap();
        let pool = api::pool().unwrap();
        let services = document["api"]["services"].as_object().unwrap();
        let defs = document["$defs"].as_object().unwrap();
        let mut methods = 0;
        for service in pool.services().filter(|s| s.package_name() == api::PACKAGE) {
            let entry = &services[service.full_name()];
            for method in service.methods() {
                let path = format!("/{}/{}", service.full_name(), method.name());
                let access = match reef_api::access(&path).unwrap() {
                    reef_api::v1::Access::Read => "read",
                    reef_api::v1::Access::Write => "write",
                    reef_api::v1::Access::Admin => "admin",
                    other @ reef_api::v1::Access::Unspecified => panic!("{path}: {other:?}"),
                };
                let method_entry = &entry["methods"][method.name()];
                assert_eq!(method_entry["path"], path);
                assert_eq!(method_entry["access"], access, "{path}");
                methods += 1;
            }
        }
        assert_eq!(methods, reef_api::methods().count());
        for message in pool.all_messages() {
            if message.package_name() == api::PACKAGE && !message.is_map_entry() {
                assert!(
                    defs.contains_key(message.full_name()),
                    "{}",
                    message.full_name()
                );
            }
        }
        // Every reference resolves, and no other well-known type is used.
        let mut names = Vec::new();
        refs(&document, &mut names);
        for name in names {
            assert!(defs.contains_key(&name), "unresolved #/$defs/{name}");
            assert!(!name.starts_with("google.protobuf."), "{name}");
        }
    }

    /// A value for `kind`, with messages filled to `depth`.
    fn sample(kind: &Kind, depth: usize, choice: usize) -> Option<Reflected> {
        Some(match kind {
            Kind::Double => Reflected::F64(1.5),
            Kind::Float => Reflected::F32(1.5),
            Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Reflected::I32(-7),
            Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Reflected::I64(-1_234_567_890_123),
            Kind::Uint32 | Kind::Fixed32 => Reflected::U32(7),
            Kind::Uint64 | Kind::Fixed64 => Reflected::U64(1_234_567_890_123),
            Kind::Bool => Reflected::Bool(true),
            Kind::String => Reflected::String("text".to_owned()),
            Kind::Bytes => Reflected::Bytes(vec![0, 255].into()),
            Kind::Enum(enumeration) => {
                Reflected::EnumNumber(enumeration.values().last().map_or(0, |v| v.number()))
            }
            Kind::Message(message) => {
                let mut value = DynamicMessage::new(message.clone());
                match message.full_name() {
                    "google.protobuf.Timestamp" => {
                        value.set_field_by_name("seconds", Reflected::I64(1_700_000_000));
                        value.set_field_by_name("nanos", Reflected::I32(5_000_000));
                    }
                    "google.protobuf.Duration" => {
                        value.set_field_by_name("seconds", Reflected::I64(90));
                        value.set_field_by_name("nanos", Reflected::I32(500_000_000));
                    }
                    _ if depth == 0 => return None,
                    _ => value = filled(message, depth - 1, choice),
                }
                Reflected::Message(value)
            }
        })
    }

    fn field_value(field: &FieldDescriptor, depth: usize, choice: usize) -> Option<Reflected> {
        if field.is_map() {
            let Kind::Message(entry) = field.kind() else {
                return None;
            };
            let key = match entry.map_entry_key_field().kind() {
                Kind::String => MapKey::String("key".to_owned()),
                Kind::Bool => MapKey::Bool(true),
                Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => MapKey::I32(3),
                Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => MapKey::I64(3),
                Kind::Uint32 | Kind::Fixed32 => MapKey::U32(3),
                _ => MapKey::U64(3),
            };
            let value = sample(&entry.map_entry_value_field().kind(), depth, choice)?;
            return Some(Reflected::Map(HashMap::from([(key, value)])));
        }
        let one = sample(&field.kind(), depth, choice)?;
        if field.is_list() {
            return Some(Reflected::List(vec![one.clone(), one]));
        }
        Some(one)
    }

    /// `message` with every field set, and of each oneof the member at
    /// `choice` (modulo its size).
    fn filled(message: &MessageDescriptor, depth: usize, choice: usize) -> DynamicMessage {
        let mut value = DynamicMessage::new(message.clone());
        for field in message.fields() {
            if let Some(oneof) = field.containing_oneof()
                && !oneof.is_synthetic()
            {
                let members: Vec<_> = oneof.fields().collect();
                if members[choice % members.len()] != field {
                    continue;
                }
            }
            if let Some(set) = field_value(&field, depth, choice) {
                value.set_field(&field, set);
            }
        }
        value
    }

    fn canonical(value: &DynamicMessage) -> Value {
        value
            .serialize_with_options(
                serde_json::value::Serializer,
                &SerializeOptions::new().skip_default_fields(false),
            )
            .unwrap()
    }

    #[test]
    fn api_schemas_accept_canonical_json() {
        let document = document().unwrap();
        let pool = api::pool().unwrap();
        let mut oneofs_checked = 0;
        for message in pool.all_messages() {
            if message.package_name() != api::PACKAGE || message.is_map_entry() {
                continue;
            }
            let validator = validator(&document, message.full_name());
            let widest = message
                .oneofs()
                .filter(|oneof| !oneof.is_synthetic())
                .map(|oneof| oneof.fields().len())
                .max()
                .unwrap_or(1);
            for choice in 0..widest {
                let json = canonical(&filled(&message, 3, choice));
                let errors: Vec<String> = validator
                    .iter_errors(&json)
                    .map(|e| e.to_string())
                    .collect();
                assert!(
                    errors.is_empty(),
                    "{}: {errors:?}\n{json}",
                    message.full_name()
                );
            }
            // Two members of a oneof together are refused.
            for oneof in message.oneofs().filter(|oneof| !oneof.is_synthetic()) {
                let members: Vec<_> = oneof.fields().collect();
                if members.len() < 2 {
                    continue;
                }
                let mut json = canonical(&filled(&message, 3, 0));
                let second = canonical(&filled(&message, 3, 1));
                let name = members[1].json_name();
                json[name] = second[name].clone();
                assert!(
                    !validator.is_valid(&json),
                    "{}: {json}",
                    message.full_name()
                );
                oneofs_checked += 1;
            }
        }
        assert!(oneofs_checked > 0);
    }

    #[test]
    fn document_is_a_valid_schema() {
        let document = document().unwrap();
        jsonschema::meta::validate(&document).unwrap();
        for name in document["$defs"].as_object().unwrap().keys() {
            validator(&document, name);
        }
        assert_eq!(document, super::document().unwrap(), "two builds differ");
    }

    #[test]
    fn brief_views_are_subsets() {
        /// A property's kind: its type without `null`, through `$ref`
        /// (without `brief.`), `anyOf` with null, and array items.
        fn kind(schema: &Value) -> String {
            if let Some(target) = schema["$ref"].as_str() {
                return target.replace("#/$defs/brief.", "#/$defs/");
            }
            if let Some([only, null]) = schema["anyOf"].as_array().map(Vec::as_slice)
                && null["type"] == "null"
            {
                return kind(only);
            }
            let types: Vec<&str> = match &schema["type"] {
                Value::String(one) => vec![one.as_str()],
                Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            let types: Vec<&str> = types.into_iter().filter(|t| *t != "null").collect();
            if types == ["array"] {
                return format!("array of {}", kind(&schema["items"]));
            }
            types.join("|")
        }
        let document = document().unwrap();
        let defs = document["$defs"].as_object().unwrap();
        let views: Vec<&String> = defs
            .keys()
            .filter(|name| name.starts_with("brief."))
            .collect();
        assert_eq!(views.len(), 8, "{views:?}");
        for name in views {
            let view = &defs[name];
            let full = &defs[name.trim_start_matches("brief.")];
            for (key, property) in view["properties"].as_object().unwrap() {
                let from = &full["properties"][key];
                assert!(!from.is_null(), "{name}.{key} is not in the full type");
                assert_eq!(kind(property), kind(from), "{name}.{key}");
                if let Some(values) = property["enum"].as_array() {
                    for value in values {
                        assert!(
                            from["enum"].as_array().unwrap().contains(value),
                            "{name}.{key}"
                        );
                    }
                }
                assert!(
                    property.get("description").is_some(),
                    "{name}.{key} has no description"
                );
            }
            assert!(
                view["description"]
                    .as_str()
                    .unwrap()
                    .ends_with("may change in a minor release."),
                "{name}"
            );
        }
        assert_eq!(
            defs["brief.Workspace"]["required"],
            serde_json::json!(["id", "owner"])
        );
    }
}
