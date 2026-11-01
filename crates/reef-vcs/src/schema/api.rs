//! `reef schema api` (R2.24): the `reef.api.v1` services and messages from
//! [`reef_api::FILE_DESCRIPTOR_SET`], as JSON Schemas of their canonical
//! proto3 JSON, the mapping in the protobuf documentation, as a printer
//! emits it: keys are each field's `json_name`, 64-bit integers are
//! decimal strings, enums are their value names, `bytes` are base64, and
//! `Timestamp` and `Duration` are RFC 3339 and `<seconds>s` strings. No
//! crate converts prost-reflect's descriptors, and the package uses a
//! handful of features, so the conversion is here.
//!
//! The v0 package is not described: it is served only to name a version
//! mismatch.

use std::collections::HashMap;

use prost_reflect::{
    DescriptorPool, EnumDescriptor, FieldDescriptor, FileDescriptor, Kind, MessageDescriptor,
    ServiceDescriptor,
};
use serde_json::{Map, Value, json};

/// The package described.
pub(crate) const PACKAGE: &str = "reef.api.v1";

/// The well-known types the package uses, inlined where referenced.
const TIMESTAMP: &str = "google.protobuf.Timestamp";
const DURATION: &str = "google.protobuf.Duration";

/// The descriptor set as a pool; `None` only if the build embedded a set
/// that does not decode, which `reef-api`'s own tests rule out.
pub(crate) fn pool() -> Option<DescriptorPool> {
    DescriptorPool::decode(reef_api::FILE_DESCRIPTOR_SET).ok()
}

/// The API part: `{services}`, and the definitions of every message and
/// enum of the package, keyed by full name, added to `defs`.
pub(crate) fn part(pool: &DescriptorPool, defs: &mut Map<String, Value>) -> Value {
    let comments = Comments::of(pool);
    for message in pool.all_messages() {
        if message.package_name() == PACKAGE && !message.is_map_entry() {
            defs.insert(
                message.full_name().to_owned(),
                message_schema(&message, &comments),
            );
        }
    }
    for enumeration in pool.all_enums() {
        if enumeration.package_name() == PACKAGE {
            defs.insert(
                enumeration.full_name().to_owned(),
                enum_schema(&enumeration, &comments),
            );
        }
    }
    let services: Map<String, Value> = pool
        .services()
        .filter(|service| service.package_name() == PACKAGE)
        .map(|service| {
            (
                service.full_name().to_owned(),
                service_entry(&service, &comments),
            )
        })
        .collect();
    json!({ "services": services })
}

/// A reference to a definition of this document.
pub(crate) fn reference(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

fn service_entry(service: &ServiceDescriptor, comments: &Comments) -> Value {
    let methods: Map<String, Value> = service
        .methods()
        .map(|method| {
            let path = format!("/{}/{}", service.full_name(), method.name());
            let access = match reef_api::access(&path) {
                Some(reef_api::v1::Access::Read) => "read",
                Some(reef_api::v1::Access::Write) => "write",
                Some(reef_api::v1::Access::Admin) => "admin",
                _ => "unknown",
            };
            let idempotency = match method
                .method_descriptor_proto()
                .options
                .as_ref()
                .map(prost_types::MethodOptions::idempotency_level)
            {
                Some(prost_types::method_options::IdempotencyLevel::NoSideEffects) => {
                    "no_side_effects"
                }
                Some(prost_types::method_options::IdempotencyLevel::Idempotent) => "idempotent",
                _ => "unknown",
            };
            let mut entry = json!({
                "path": path,
                "input": reference(method.input().full_name()),
                "output": reference(method.output().full_name()),
                "client_streaming": method.is_client_streaming(),
                "server_streaming": method.is_server_streaming(),
                "access": access,
                "idempotency": idempotency,
            });
            describe(
                &mut entry,
                comments.get(&method.parent_file(), method.path()),
            );
            (method.name().to_owned(), entry)
        })
        .collect();
    let mut entry = json!({ "methods": methods });
    describe(
        &mut entry,
        comments.get(&service.parent_file(), service.path()),
    );
    entry
}

fn message_schema(message: &MessageDescriptor, comments: &Comments) -> Value {
    let file = message.parent_file();
    let properties: Map<String, Value> = message
        .fields()
        .map(|field| {
            let mut schema = field_schema(&field);
            describe(&mut schema, comments.get(&file, field.path()));
            (field.json_name().to_owned(), schema)
        })
        .collect();
    let mut schema = json!({ "type": "object", "properties": properties });
    // At most one member of each oneof: no two of its keys together.
    // Proto3 `optional` is a synthetic oneof of one field, and not one.
    let exclusions: Vec<Value> = message
        .oneofs()
        .filter(|oneof| !oneof.is_synthetic())
        .flat_map(|oneof| {
            let members: Vec<String> = oneof
                .fields()
                .map(|field| field.json_name().to_owned())
                .collect();
            let mut pairs = Vec::new();
            for (index, first) in members.iter().enumerate() {
                for second in &members[index + 1..] {
                    pairs.push(json!({ "not": { "required": [first, second] } }));
                }
            }
            pairs
        })
        .collect();
    if !exclusions.is_empty() {
        schema["allOf"] = Value::Array(exclusions);
    }
    describe(&mut schema, comments.get(&file, message.path()));
    schema
}

fn enum_schema(enumeration: &EnumDescriptor, comments: &Comments) -> Value {
    let values: Vec<String> = enumeration
        .values()
        .map(|value| value.name().to_owned())
        .collect();
    let mut schema = json!({ "type": "string", "enum": values });
    describe(
        &mut schema,
        comments.get(&enumeration.parent_file(), enumeration.path()),
    );
    schema
}

fn field_schema(field: &FieldDescriptor) -> Value {
    if field.is_map() {
        let Kind::Message(entry) = field.kind() else {
            return json!({ "type": "object" });
        };
        return json!({
            "type": "object",
            "additionalProperties": kind_schema(&entry.map_entry_value_field().kind()),
        });
    }
    if field.is_list() {
        return json!({ "type": "array", "items": kind_schema(&field.kind()) });
    }
    kind_schema(&field.kind())
}

fn kind_schema(kind: &Kind) -> Value {
    match kind {
        Kind::Double | Kind::Float => json!({ "type": "number" }),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            json!({ "type": "integer", "minimum": i32::MIN, "maximum": i32::MAX })
        }
        Kind::Uint32 | Kind::Fixed32 => {
            json!({ "type": "integer", "minimum": 0, "maximum": u32::MAX })
        }
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => {
            json!({ "type": "string", "pattern": "^-?[0-9]+$" })
        }
        Kind::Uint64 | Kind::Fixed64 => json!({ "type": "string", "pattern": "^[0-9]+$" }),
        Kind::Bool => json!({ "type": "boolean" }),
        Kind::String => json!({ "type": "string" }),
        Kind::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
        Kind::Message(message) => match message.full_name() {
            TIMESTAMP => json!({ "type": "string", "format": "date-time" }),
            DURATION => json!({ "type": "string", "pattern": "^-?[0-9]+(\\.[0-9]{1,9})?s$" }),
            name => reference(name),
        },
        Kind::Enum(enumeration) => reference(enumeration.full_name()),
    }
}

/// Adds `description` when there is one.
fn describe(schema: &mut Value, comment: Option<&str>) {
    if let (Some(comment), Value::Object(object)) = (comment, schema) {
        object.insert("description".to_owned(), Value::from(comment));
    }
}

/// Every element's comment, by file and descriptor path.
struct Comments(HashMap<(String, Vec<i32>), String>);

impl Comments {
    fn of(pool: &DescriptorPool) -> Self {
        let mut comments = HashMap::new();
        for file in pool.files() {
            let Some(info) = &file.file_descriptor_proto().source_code_info else {
                continue;
            };
            for location in &info.location {
                // The comment above an element, else the one after it.
                let text = [&location.leading_comments, &location.trailing_comments]
                    .into_iter()
                    .flatten()
                    .map(|raw| comment_text(raw))
                    .find(|text| !text.is_empty());
                if let Some(text) = text {
                    comments.insert((file.name().to_owned(), location.path.clone()), text);
                }
            }
        }
        Self(comments)
    }

    fn get(&self, file: &FileDescriptor, path: &[i32]) -> Option<&str> {
        self.0
            .get(&(file.name().to_owned(), path.to_vec()))
            .map(String::as_str)
    }
}

/// A comment as written, without the space after each `//` and the
/// newlines around it.
fn comment_text(raw: &str) -> String {
    raw.lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line).trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}
