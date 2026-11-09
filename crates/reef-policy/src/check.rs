//! Reef's static check of an expression, since cel-rust has no type
//! checker.
//!
//! It walks the AST after parsing and refuses:
//!
//! - an identifier that is neither a variable of the expression nor bound
//!   by an enclosing comprehension;
//! - a field selected on a value of known shape (the input's, followed
//!   through select chains and comprehension ranges) that the shape does
//!   not have; below `annotations`, and anything else of unknown shape,
//!   nothing is checked;
//! - a call to a function that is neither CEL's standard library nor
//!   Reef's, and a call to Reef's with the wrong arguments or a literal
//!   pattern that does not compile;
//! - a comprehension over a map, whose order cel-rust leaves undefined;
//! - message literals, which name types this environment does not have;
//! - an expression over [`MAX_NODES`] nodes.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use cel::IdedExpr;
use cel::common::ast::{EntryExpr, Expr};
use serde_json::Value as Json;

use crate::env::{REEF_FUNCTIONS, STANDARD_FUNCTIONS, TYPE_NAMES};
use crate::input::{ConflictInput, Evidence, Input};

/// The operators cel-rust's parser and macros produce, by their function
/// names: any other name is a call the static check looks up.
const OPERATORS: [&str; 21] = [
    "_?_:_",
    "_&&_",
    "_||_",
    "!_",
    "_-_",
    "_+_",
    "_*_",
    "_/_",
    "_%_",
    "_==_",
    "_!=_",
    "_>=_",
    "_<=_",
    "_>_",
    "_<_",
    "-_",
    "_[_]",
    "_[?_]",
    "_?._",
    "@not_strictly_false",
    "@in",
];

/// The most nodes an expression may have after its macros are expanded.
pub const MAX_NODES: usize = 1_000;

/// What is known about a value's structure.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Shape {
    /// Nothing: any field or call is allowed.
    Any,
    /// A string, number or boolean.
    Scalar,
    /// A list of elements of one shape.
    List(Box<Shape>),
    /// A map from strings to values of one shape.
    Map(Box<Shape>),
    /// A struct with these fields.
    Object(BTreeMap<String, Shape>),
}

impl Shape {
    /// The shape of `schema`, a JSON Schema schemars wrote, with its
    /// definitions in `defs`.
    fn of_schema(schema: &Json, defs: &serde_json::Map<String, Json>) -> Shape {
        if let Some(reference) = schema.get("$ref").and_then(Json::as_str) {
            return reference
                .strip_prefix("#/$defs/")
                .and_then(|name| defs.get(name))
                .map_or(Shape::Any, |schema| Shape::of_schema(schema, defs));
        }
        match schema.get("type").and_then(Json::as_str) {
            Some("object") => match schema.get("properties").and_then(Json::as_object) {
                Some(properties) => Shape::Object(
                    properties
                        .iter()
                        .map(|(name, schema)| (name.clone(), Shape::of_schema(schema, defs)))
                        .collect(),
                ),
                None => Shape::Map(Box::new(
                    schema
                        .get("additionalProperties")
                        .filter(|value| value.is_object())
                        .map_or(Shape::Any, |schema| Shape::of_schema(schema, defs)),
                )),
            },
            Some("array") => Shape::List(Box::new(
                schema
                    .get("items")
                    .map_or(Shape::Any, |schema| Shape::of_schema(schema, defs)),
            )),
            Some("string" | "integer" | "number" | "boolean") => Shape::Scalar,
            _ => Shape::Any,
        }
    }

    /// The shape of a type's JSON Schema.
    fn of<T: schemars::JsonSchema>() -> Shape {
        let schema = schemars::schema_for!(T);
        let value = schema.as_value();
        let defs = value
            .get("$defs")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default();
        Shape::of_schema(value, &defs)
    }

    /// The element shape when this is a list.
    pub(crate) fn element(&self) -> Option<&Shape> {
        match self {
            Shape::List(element) => Some(element),
            _ => None,
        }
    }
}

/// The variables each kind of expression sees, and their shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A `[[require]]`, `[[forbid]]`, `[[signoff]]` or `[[rule]]`
    /// expression: `change`, `evidence`, `provenance` and `groups`.
    Change,
    /// An attestor set's `when`: `e` (one evidence object) and `groups`.
    Evidence,
    /// A `[[conflicts.rule]]`'s `when`: `conflict` and `groups`.
    Conflict,
}

/// The shapes of the variables, built once from the input's schema.
pub(crate) struct Shapes {
    pub(crate) change: Shape,
    pub(crate) evidence: Shape,
    pub(crate) provenance: Shape,
    pub(crate) landing: Shape,
    pub(crate) groups: Shape,
    pub(crate) e: Shape,
    pub(crate) conflict: Shape,
}

pub(crate) fn shapes() -> &'static Shapes {
    static SHAPES: OnceLock<Shapes> = OnceLock::new();
    SHAPES.get_or_init(|| {
        let input = Shape::of::<Input>();
        let field = |name: &str| match &input {
            Shape::Object(fields) => fields.get(name).cloned().unwrap_or(Shape::Any),
            _ => Shape::Any,
        };
        Shapes {
            change: field("change"),
            evidence: field("evidence"),
            provenance: field("provenance"),
            landing: field("landing"),
            groups: Shape::Map(Box::new(Shape::List(Box::new(Shape::Scalar)))),
            e: Shape::of::<Evidence>(),
            conflict: Shape::of::<ConflictInput>(),
        }
    })
}

impl Scope {
    /// Its variables, with their shapes and their paths in the input.
    pub(crate) fn variables(self) -> Vec<(&'static str, &'static Shape, &'static str)> {
        let shapes = shapes();
        match self {
            Scope::Change => vec![
                ("change", &shapes.change, "change"),
                ("evidence", &shapes.evidence, "evidence"),
                ("provenance", &shapes.provenance, "provenance"),
                ("landing", &shapes.landing, "landing"),
                ("groups", &shapes.groups, "groups"),
            ],
            Scope::Evidence => vec![
                ("e", &shapes.e, "evidence.*"),
                ("groups", &shapes.groups, "groups"),
            ],
            Scope::Conflict => vec![
                ("conflict", &shapes.conflict, "conflict"),
                ("groups", &shapes.groups, "groups"),
            ],
        }
    }

    fn names(self) -> String {
        self.variables()
            .iter()
            .map(|(name, _, _)| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One refusal: the node it is about and why.
#[derive(Debug, PartialEq)]
pub(crate) struct Problem {
    pub(crate) node: u64,
    pub(crate) message: String,
}

/// What [`check`] found: every problem, and the expression's node count.
#[derive(Debug)]
pub(crate) struct Checked {
    pub(crate) problems: Vec<Problem>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) nodes: usize,
}

/// Checks `expr` for `scope`.
pub(crate) fn check(expr: &IdedExpr, scope: Scope) -> Checked {
    let mut checker = Checker {
        scope,
        variables: scope.variables(),
        bound: Vec::new(),
        problems: Vec::new(),
        nodes: 0,
    };
    checker.walk(expr);
    if checker.nodes > MAX_NODES {
        checker.problems.insert(
            0,
            Problem {
                node: expr.id,
                message: format!(
                    "the expression has {} nodes after its macros are expanded; at most {MAX_NODES}",
                    checker.nodes
                ),
            },
        );
    }
    Checked {
        problems: checker.problems,
        nodes: checker.nodes,
    }
}

struct Checker {
    scope: Scope,
    variables: Vec<(&'static str, &'static Shape, &'static str)>,
    bound: Vec<(String, Shape)>,
    problems: Vec<Problem>,
    nodes: usize,
}

impl Checker {
    fn problem(&mut self, node: u64, message: String) {
        self.problems.push(Problem { node, message });
    }

    fn walk(&mut self, expr: &IdedExpr) -> Shape {
        self.nodes += 1;
        match &expr.expr {
            Expr::Unspecified => Shape::Any,
            Expr::Literal(_) => Shape::Scalar,
            Expr::Ident(name) => self.ident(expr.id, name),
            Expr::Select(select) => {
                let operand = self.walk(&select.operand);
                let field = self.field(expr.id, &operand, &select.field);
                if select.test { Shape::Scalar } else { field }
            }
            Expr::Call(call) => self.call(expr, call),
            Expr::List(list) => {
                let shapes: Vec<Shape> = list.elements.iter().map(|e| self.walk(e)).collect();
                let element = match shapes.split_first() {
                    Some((first, rest)) if rest.iter().all(|shape| shape == first) => first.clone(),
                    _ => Shape::Any,
                };
                Shape::List(Box::new(element))
            }
            Expr::Map(map) => {
                for entry in &map.entries {
                    if let EntryExpr::MapEntry(entry) = &entry.expr {
                        self.walk(&entry.key);
                        self.walk(&entry.value);
                    }
                }
                Shape::Map(Box::new(Shape::Any))
            }
            Expr::Struct(literal) => {
                self.problem(
                    expr.id,
                    format!(
                        "`{} {{...}}` is a message literal; policy expressions have no message types",
                        literal.type_name
                    ),
                );
                for entry in &literal.entries {
                    if let EntryExpr::StructField(field) = &entry.expr {
                        self.walk(&field.value);
                    }
                }
                Shape::Any
            }
            Expr::Comprehension(comprehension) => {
                let range = self.walk(&comprehension.iter_range);
                if let Shape::Map(_) = range {
                    self.problem(
                        comprehension.iter_range.id,
                        "a macro over a map: its iteration order is undefined, so a policy may only iterate lists".to_owned(),
                    );
                }
                // `map` and `filter` (an accumulator that starts as a list)
                // expose their range's order: over a value not known to be
                // a list, which may be a map at run time, the result could
                // differ between runs (review finding 5).
                let builds = matches!(comprehension.accu_init.expr, Expr::List(_));
                if builds && !matches!(range, Shape::List(_) | Shape::Map(_)) {
                    self.problem(
                        comprehension.iter_range.id,
                        "`map` or `filter` over a value not known to be a list: a map's order is undefined, so a policy maps and filters only lists (the input's lists, list literals, `matching`, and other maps' and filters' results)".to_owned(),
                    );
                }
                let element = match &range {
                    Shape::List(element) => (**element).clone(),
                    _ => Shape::Any,
                };
                self.walk(&comprehension.accu_init);
                let depth = self.bound.len();
                match &comprehension.iter_var2 {
                    Some(second) => {
                        self.bound
                            .push((comprehension.iter_var.clone(), Shape::Scalar));
                        self.bound.push((second.clone(), element));
                    }
                    None => self.bound.push((comprehension.iter_var.clone(), element)),
                }
                self.bound
                    .push((comprehension.accu_var.clone(), Shape::Any));
                self.walk(&comprehension.loop_cond);
                self.walk(&comprehension.loop_step);
                self.bound.truncate(depth);
                self.bound
                    .push((comprehension.accu_var.clone(), Shape::Any));
                let result = self.walk(&comprehension.result);
                self.bound.truncate(depth);
                if builds {
                    Shape::List(Box::new(Shape::Any))
                } else {
                    result
                }
            }
        }
    }

    fn ident(&mut self, node: u64, name: &str) -> Shape {
        if let Some((_, shape)) = self.bound.iter().rev().find(|(bound, _)| bound == name) {
            return shape.clone();
        }
        if let Some((_, shape, _)) = self.variables.iter().find(|(var, _, _)| *var == name) {
            return (*shape).clone();
        }
        if TYPE_NAMES.contains(&name) {
            return Shape::Any;
        }
        let hint = match (self.scope, name) {
            (Scope::Change, "e") => {
                "; `e` is an attestor set's `when` variable: iterate `evidence` instead"
            }
            (Scope::Evidence, "change" | "evidence" | "provenance" | "landing") => {
                "; an attestor set's `when` sees one evidence object, `e`"
            }
            _ => "",
        };
        self.problem(
            node,
            format!(
                "unknown variable `{name}`: this expression sees {}{hint}",
                self.scope.names()
            ),
        );
        Shape::Any
    }

    fn field(&mut self, node: u64, operand: &Shape, field: &str) -> Shape {
        match operand {
            Shape::Any => Shape::Any,
            Shape::Map(value) => (**value).clone(),
            Shape::Object(fields) => {
                if let Some(shape) = fields.get(field) {
                    shape.clone()
                } else {
                    let known: Vec<&str> = fields.keys().map(String::as_str).collect();
                    self.problem(
                        node,
                        format!(
                            "no field `{field}` here; the fields are {}",
                            known.join(", ")
                        ),
                    );
                    Shape::Any
                }
            }
            Shape::List(_) => {
                self.problem(
                    node,
                    format!(
                        "`.{field}` on a list: select it on an element, inside a macro such as `all` or `exists`"
                    ),
                );
                Shape::Any
            }
            Shape::Scalar => {
                self.problem(
                    node,
                    format!("`.{field}` on a string, number or boolean, which has no fields"),
                );
                Shape::Any
            }
        }
    }

    fn call(&mut self, expr: &IdedExpr, call: &cel::common::ast::CallExpr) -> Shape {
        let name = call.func_name.as_str();
        // `optional.of(x)` and the like: a call on the `optional` namespace.
        let namespaced = matches!(
            call.target.as_deref().map(|target| &target.expr),
            Some(Expr::Ident(target)) if target == "optional"
        );
        let target = if namespaced {
            let qualified = format!("optional.{name}");
            if !STANDARD_FUNCTIONS.contains(&qualified.as_str()) {
                self.problem(expr.id, format!("unknown function `{qualified}`"));
            }
            None
        } else {
            call.target.as_deref().map(|target| self.walk(target))
        };
        let args: Vec<Shape> = call.args.iter().map(|arg| self.walk(arg)).collect();
        let operator = OPERATORS.contains(&name);
        if name == "matches" {
            // A literal pattern is compiled at load, as it will be at run time.
            let pattern = call
                .args
                .last()
                .filter(|_| call.args.len() == 1 || call.args.len() == 2);
            if let Some(IdedExpr {
                id,
                expr: Expr::Literal(cel::common::ast::LiteralValue::String(text)),
            }) = pattern
                && let Err(error) = crate::env::regex(text.as_ref())
            {
                self.problem(*id, format!("pattern {:?}: {error}", text.as_ref() as &str));
            }
        }
        if REEF_FUNCTIONS.contains(&name) {
            self.reef_call(expr, call);
        } else if !operator && !namespaced && !STANDARD_FUNCTIONS.contains(&name) {
            self.problem(
                expr.id,
                format!(
                    "unknown function `{name}`: the standard library's and {} are available",
                    REEF_FUNCTIONS
                        .iter()
                        .map(|f| format!("`{f}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        match name {
            "_[_]" => match args.first() {
                Some(Shape::List(element) | Shape::Map(element)) => (**element).clone(),
                _ => Shape::Any,
            },
            "matching" => Shape::List(Box::new(Shape::Scalar)),
            "_+_" if args.iter().all(|shape| matches!(shape, Shape::List(_))) => {
                Shape::List(Box::new(Shape::Any))
            }
            "glob" | "any_glob" | "all_glob" | "size" | "int" | "uint" | "double" | "string"
            | "bool" | "bytes" | "contains" | "startsWith" | "endsWith" | "matches" => {
                let _ = target;
                Shape::Scalar
            }
            _ => Shape::Any,
        }
    }

    fn reef_call(&mut self, expr: &IdedExpr, call: &cel::common::ast::CallExpr) {
        let name = call.func_name.as_str();
        if call.target.is_some() || call.args.len() != 2 {
            let form = if name == "glob" {
                "glob(path, pattern)"
            } else {
                "(paths, patterns)"
            };
            self.problem(
                expr.id,
                format!(
                    "`{name}` is called as `{}`",
                    form.replace("(paths", &format!("{name}(paths"))
                ),
            );
            return;
        }
        let literals: Vec<(u64, &str)> = match (&call.args[1].expr, name) {
            (Expr::Literal(cel::common::ast::LiteralValue::String(text)), "glob") => {
                vec![(call.args[1].id, text.as_ref())]
            }
            (Expr::List(list), "any_glob" | "all_glob" | "matching") => list
                .elements
                .iter()
                .filter_map(|element| match &element.expr {
                    Expr::Literal(cel::common::ast::LiteralValue::String(text)) => {
                        Some((element.id, text.as_ref()))
                    }
                    _ => None,
                })
                .collect(),
            (Expr::Literal(_), _) if name != "glob" => {
                self.problem(
                    call.args[1].id,
                    format!("`{name}` takes a list of patterns"),
                );
                Vec::new()
            }
            _ => Vec::new(),
        };
        for (node, pattern) in literals {
            if let Err(error) = crate::glob::compile(pattern) {
                self.problem(node, format!("pattern {pattern:?}: {error}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::parse;

    fn problems(source: &str, scope: Scope) -> Vec<String> {
        let parsed = parse(source).unwrap();
        check(&parsed.ast, scope)
            .problems
            .into_iter()
            .map(|problem| problem.message)
            .collect()
    }

    #[test]
    fn known_shapes_are_followed() {
        let shapes = shapes();
        let Shape::Object(change) = &shapes.change else {
            panic!("{:?}", shapes.change)
        };
        assert_eq!(change["paths"], Shape::List(Box::new(Shape::Scalar)));
        let Shape::Object(e) = &shapes.e else {
            panic!()
        };
        assert_eq!(e["annotations"], Shape::Map(Box::new(Shape::Any)));
        assert!(matches!(&e["key"], Shape::Object(key) if key.contains_key("identity_kind")));
        assert!(e.contains_key("self"));
    }

    #[test]
    fn good_expressions_pass() {
        for source in [
            "change.paths.all(p, !p.startsWith('secrets/'))",
            "change.files.exists(f, f.lines_added > 100 && f.status == 'added')",
            "evidence.exists(e, 'ci' in e.attestors && e.kind == 'build')",
            "provenance.all(r, r.signature == 'valid' && r.key.status == 'bound')",
            "size(groups.maintainers) > 0 && 'alice' in groups['maintainers']",
            "any_glob(change.paths, ['docs/**', '*.md'])",
            "matching(change.paths, ['crates/**']).size() < 10",
            "has(change.description) && change.description.matches('^R[0-9]')",
            "type(change.lines_added) == int",
            "optional.of(1).hasValue()",
            "evidence.map(x, x.kind).exists(k, k == 'test')",
            "[1, 2].exists_one(x, x == 1)",
        ] {
            assert_eq!(
                problems(source, Scope::Change),
                Vec::<String>::new(),
                "{source}"
            );
        }
        for source in [
            "e.fresh && e.artifacts.exists(a, a.state == 'stored')",
            "e.annotations.sandbox.kind == 'bubblewrap'",
            "has(e.annotations.sandbox)",
            "e.key.identity in groups.maintainers",
        ] {
            assert_eq!(
                problems(source, Scope::Evidence),
                Vec::<String>::new(),
                "{source}"
            );
        }
        assert_eq!(
            problems("conflict.paths.all(p, glob(p, 'docs/**'))", Scope::Conflict),
            [] as [String; 0]
        );
    }

    #[test]
    fn bad_expressions_are_refused() {
        for (source, scope, wanted) in [
            (
                "chnage.paths.size() > 0",
                Scope::Change,
                "unknown variable `chnage`",
            ),
            ("change.pahts.size() > 0", Scope::Change, "no field `pahts`"),
            (
                "change.files.all(f, f.lines > 0)",
                Scope::Change,
                "no field `lines`",
            ),
            (
                "e.key.idnetity == 'x'",
                Scope::Evidence,
                "no field `idnetity`",
            ),
            ("e.fresh", Scope::Change, "unknown variable `e`"),
            (
                "change.paths.size() > 0",
                Scope::Evidence,
                "unknown variable `change`",
            ),
            (
                "lowerAscii('A') == 'a'",
                Scope::Change,
                "unknown function `lowerAscii`",
            ),
            ("groups.all(g, g != '')", Scope::Change, "over a map"),
            (
                "e.annotations.all(k, k != '')",
                Scope::Evidence,
                "over a map",
            ),
            ("change.paths.kind == 'x'", Scope::Change, "on a list"),
            ("glob(change.id, 'a/[')", Scope::Change, "pattern \"a/[\""),
            (
                "any_glob(change.paths, 'docs/**')",
                Scope::Change,
                "takes a list",
            ),
            ("glob(change.id)", Scope::Change, "is called as"),
            ("Foo{a: 1} == 1", Scope::Change, "message literal"),
            ("_nosuch(1)", Scope::Change, "unknown function `_nosuch`"),
            (
                "dyn(groups).map(k, k)[0] == 'a'",
                Scope::Change,
                "not known to be a list",
            ),
            (
                "e.annotations.x.map(k, k).size() > 0",
                Scope::Evidence,
                "not known to be a list",
            ),
            ("change.id.matches('(')", Scope::Change, "pattern \"(\""),
            (
                "optional.nope(1)",
                Scope::Change,
                "unknown function `optional.nope`",
            ),
        ] {
            let found = problems(source, scope);
            assert!(
                found.iter().any(|message| message.contains(wanted)),
                "{source}: {found:?}"
            );
        }
    }

    #[test]
    fn large_expressions_are_refused() {
        let source = vec!["change.empty"; 600].join(" || ");
        let parsed = parse(&source).unwrap();
        let checked = check(&parsed.ast, Scope::Change);
        assert!(checked.nodes > MAX_NODES);
        assert!(checked.problems[0].message.contains("nodes"));
    }
}
