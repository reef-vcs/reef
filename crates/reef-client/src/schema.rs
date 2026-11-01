//! JSON Schemas of [`json`]'s and [`brief`]'s types (R2.24, ADR 0050),
//! which `reef schema` publishes. Every output type derives
//! [`schemars::JsonSchema`] under [`settings`], the serialize contract, so
//! a schema says what serde prints: an `Option` field without
//! `skip_serializing_if` is required and may be `null`, one with it is
//! optional, and `#[serde(skip)]` fields are absent. Nothing sets
//! `additionalProperties: false`, so a newer Reef's added keys (ADR 0034)
//! never fail an older schema.
//!
//! A string field filled by a name function, such as
//! [`json::state_name`], lists its values from that function ([`names`]),
//! so the list cannot drift from what prints.
//!
//! [`json`]: crate::json
//! [`brief`]: crate::brief
//! [`json::state_name`]: crate::json::state_name

use schemars::generate::SchemaSettings;
use schemars::{Schema, json_schema};
use serde_json::{Map, Value};

use crate::json;

/// How every output type's schema is generated: JSON Schema 2020-12, as
/// the types serialize.
#[must_use]
pub fn settings() -> SchemaSettings {
    SchemaSettings::draft2020_12().for_serialize()
}

/// A name function's values: `{"type": "string", "enum": [...]}`, with
/// `null` too when `nullable`, from `name` over every value a field of the
/// API can hold today (0 to 255) and one no build knows (`i32::MAX`, which
/// prints as `unknown`), in first-seen order.
pub fn names(name: impl Fn(i32) -> Option<&'static str>, nullable: bool) -> Schema {
    let mut values: Vec<Value> = Vec::new();
    for value in (0..=255).chain([i32::MAX]) {
        if let Some(text) = name(value)
            && !values.iter().any(|seen| seen == text)
        {
            values.push(Value::from(text));
        }
    }
    if nullable {
        values.push(Value::Null);
        json_schema!({"type": ["string", "null"], "enum": values})
    } else {
        json_schema!({"type": "string", "enum": values})
    }
}

/// A name function's result as an `Option`, so [`names`] takes both kinds.
pub trait Name {
    /// The name, if any.
    fn name(self) -> Option<&'static str>;
}

impl Name for &'static str {
    fn name(self) -> Option<&'static str> {
        Some(self)
    }
}

impl Name for Option<&'static str> {
    fn name(self) -> Option<&'static str> {
        self
    }
}

/// The prefix of a brief view's definition name.
pub const BRIEF_PREFIX: &str = "brief.";

/// What every brief view's description ends with (ADR 0045 decision 5).
const BRIEF_NOTE: &str = "Which keys this view keeps may change in a minor release.";

/// The full types the brief views borrow, by definition name.
fn full_types() -> Map<String, Value> {
    let mut generator = settings().into_generator();
    generator.subschema_for::<json::Workspace>();
    generator.subschema_for::<json::Diff>();
    generator.subschema_for::<json::RepoStatus>();
    generator.take_definitions(true)
}

/// Gives each brief view in `defs` (`brief.<Name>`) its full type's
/// descriptions and value lists: a view's fields carry no doc comments,
/// and ADR 0045's subset rule makes each of its keys the full type's same
/// key with the same value. A view never prints `null` (rule 2), so its
/// fields lose it. A view's own description gains a note that its keys
/// may change in a minor release.
pub fn describe_brief(defs: &mut Map<String, Value>) {
    let full_defs = full_types();
    let names: Vec<String> = defs
        .keys()
        .filter(|name| name.starts_with(BRIEF_PREFIX))
        .cloned()
        .collect();
    for name in names {
        if let Some(Value::Object(view)) = defs.get_mut(&name) {
            describe_view(view, &full_defs, name.trim_start_matches(BRIEF_PREFIX));
        }
    }
}

/// [`describe_brief`] for one schema, a view of the full type `full`
/// (`RepoStatus`) or one a view is flattened into: the root of an MCP
/// tool's output schema, which is not among its `$defs`.
pub fn describe_view_root(schema: &mut Value, full: &str) {
    if let Value::Object(view) = schema {
        describe_view(view, &full_types(), full);
    }
}

fn describe_view(view: &mut Map<String, Value>, full_defs: &Map<String, Value>, full: &str) {
    let Some(Value::Object(full_properties)) =
        full_defs.get(full).and_then(|def| def.get("properties"))
    else {
        return;
    };
    let description = match view.get("description").and_then(Value::as_str) {
        Some(text) => format!("{text} {BRIEF_NOTE}"),
        None => BRIEF_NOTE.to_owned(),
    };
    view.insert("description".to_owned(), Value::from(description));
    let Some(Value::Object(properties)) = view.get_mut("properties") else {
        return;
    };
    for (key, property) in properties.iter_mut() {
        let (Some(Value::Object(from)), Value::Object(to)) = (full_properties.get(key), property)
        else {
            continue;
        };
        for keyword in ["description", "enum"] {
            if let Some(value) = from.get(keyword)
                && !to.contains_key(keyword)
            {
                to.insert(keyword.to_owned(), value.clone());
            }
        }
        // A view leaves `null` out (rule 2).
        if let Some(Value::Array(values)) = to.get_mut("enum") {
            values.retain(|value| !value.is_null());
        }
        if let Some(Value::Array(types)) = to.get_mut("type") {
            types.retain(|kind| kind != "null");
            if let [only] = types.as_slice() {
                let only = only.clone();
                to.insert("type".to_owned(), only);
            }
        }
        if let Some(Value::Array(any)) = to.get("anyOf")
            && let [only, null] = any.as_slice()
            && null == &serde_json::json!({"type": "null"})
            && let Value::Object(only) = only.clone()
        {
            to.remove("anyOf");
            to.extend(only);
        }
    }
}

/// Rewrites rustdoc's links in every `description` under `value` as plain
/// Markdown: `` [`Workspace`] `` prints as `` `Workspace` ``, and
/// `[text](target)` or `[text][target]` as `text`.
pub fn strip_doc_links(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(text) if key == "description" => *text = plain(text),
                    _ => strip_doc_links(value),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_doc_links),
        _ => {}
    }
}

/// `text` with its rustdoc links as their text.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &after[..close];
        let mut tail = &after[close + 1..];
        // `[text](target)` and `[text][target]` keep their text.
        let mut link = false;
        for (start, end) in [('(', ')'), ('[', ']')] {
            if let Some(target) = tail.strip_prefix(start)
                && let Some(end) = target.find(end)
            {
                tail = &target[end + 1..];
                link = true;
                break;
            }
        }
        let code = inner.len() >= 2 && inner.starts_with('`') && inner.ends_with('`');
        if link || code {
            out.push_str(inner);
        } else {
            // Not a link: `[]`, `[default: 30s]`.
            out.push('[');
            out.push_str(inner);
            out.push(']');
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(schema: &Schema) -> Vec<Value> {
        schema
            .get("enum")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn names_come_from_the_name_functions() {
        let states = names(|value| json::state_name(value).name(), false);
        assert_eq!(
            values(&states),
            ["unknown", "active", "releasing", "released", "reclaimed"]
        );
        assert_eq!(states.get("type"), Some(&Value::from("string")));
        let attention = names(|value| json::attention_name(value).name(), true);
        let attention = values(&attention);
        assert!(attention.contains(&Value::from("stale-recovered")));
        assert!(attention.contains(&Value::from(json::UNKNOWN)));
        assert_eq!(attention.last(), Some(&Value::Null));
        let never_unknown = names(|value| (value == 1).then_some("one"), false);
        assert_eq!(values(&never_unknown), ["one"]);
    }

    #[test]
    fn doc_links_print_as_their_text() {
        assert_eq!(plain("the [`json`] form"), "the `json` form");
        assert_eq!(plain("see [`Out`](crate::cli::Out)."), "see `Out`.");
        assert_eq!(plain("a [b][c] d"), "a b d");
        assert_eq!(plain("open [ only"), "open [ only");
        assert_eq!(
            plain("`[]` and [default: 30s] stay"),
            "`[]` and [default: 30s] stay"
        );
    }
}
