//! The cost bound: an upper bound on what evaluating an expression over
//! one input costs, computed before it runs, since cel-rust tracks no cost.
//!
//! Every AST node costs 1. Reef's list functions cost the length of their
//! `paths`, and `in`, and `+` and `==` on lists, the bound of their list
//! operands. A comprehension costs its range, accumulator and result once,
//! plus `bound(range) × cost(loop condition and step)`.
//!
//! `bound` is how long a list can be:
//!
//! - a list literal: its length;
//! - a select chain from a variable, or from a comprehension variable of
//!   known path: the longest list at that path in this input
//!   (`change.paths`: its length; `e.artifacts`: the most artifacts any
//!   evidence object has);
//! - `x.map(…)`, `x.filter(…)`, any other comprehension, and
//!   `matching(x, …)`: `bound(x)`; `a + b`: the sum; `c ? a : b`: the
//!   larger;
//! - anything else: the input's total list elements plus the expression's
//!   literal ones.
//!
//! The bound is part of format 1, so two implementations agree on what is
//! too costly ([`MAX_EXPRESSION_COST`], [`MAX_EVALUATION_COST`]).

use std::collections::HashMap;

use cel::IdedExpr;
use cel::common::ast::{EntryExpr, Expr, LiteralValue};
use serde_json::Value as Json;

use crate::check::{Scope, Shape};

/// The most one expression may cost over one input.
pub const MAX_EXPRESSION_COST: u64 = 10_000_000;

/// The most one evaluation may cost, over all its expressions.
pub const MAX_EVALUATION_COST: u64 = 50_000_000;

/// The lists and strings of one input: the longest list and the widest
/// string at each path (a string's width counted at every path above it
/// too), the total number of list elements, and the widest string
/// anywhere.
#[derive(Debug, Default)]
pub(crate) struct Sizes {
    longest: HashMap<String, u64>,
    widest: HashMap<String, u64>,
    total: u64,
    widest_anywhere: u64,
}

impl Sizes {
    /// The sizes of `variables`, each a value (as JSON) with its shape and
    /// its path.
    pub(crate) fn of(variables: &[(&str, &Json, &Shape)]) -> Sizes {
        let mut sizes = Sizes::default();
        for (path, value, shape) in variables {
            sizes.record(value, shape, path);
        }
        sizes
    }

    fn widen(&mut self, path: &str, width: u64) {
        self.widest_anywhere = self.widest_anywhere.max(width);
        let mut at = path;
        loop {
            let widest = self.widest.entry(at.to_owned()).or_default();
            *widest = (*widest).max(width);
            match at.rfind('.') {
                Some(dot) => at = &at[..dot],
                None => break,
            }
        }
    }

    fn record(&mut self, value: &Json, shape: &Shape, path: &str) {
        match value {
            Json::Array(items) => {
                let len = items.len() as u64;
                let mut at = path;
                loop {
                    let longest = self.longest.entry(at.to_owned()).or_default();
                    *longest = (*longest).max(len);
                    match at.rfind('.') {
                        Some(dot) => at = &at[..dot],
                        None => break,
                    }
                }
                self.total = self.total.saturating_add(len);
                let element = shape.element().unwrap_or(&Shape::Any);
                let path = format!("{path}.*");
                for item in items {
                    self.record(item, element, &path);
                }
            }
            Json::Object(fields) => match shape {
                Shape::Object(shapes) => {
                    for (name, value) in fields {
                        let shape = shapes.get(name).unwrap_or(&Shape::Any);
                        self.record(value, shape, &format!("{path}.{name}"));
                    }
                }
                Shape::Map(element) => {
                    let path = format!("{path}.*");
                    for (key, value) in fields {
                        self.widen(&path, key.len() as u64);
                        self.record(value, element, &path);
                    }
                }
                _ => {
                    let path = format!("{path}.*");
                    for (key, value) in fields {
                        self.widen(&path, key.len() as u64);
                        self.record(value, &Shape::Any, &path);
                    }
                }
            },
            Json::String(text) => self.widen(path, text.len() as u64),
            _ => {}
        }
    }

    /// The longest list at or below `path`.
    fn longest(&self, path: &str) -> u64 {
        self.longest.get(path).copied().unwrap_or(0)
    }

    /// The widest string at or below `path`.
    fn widest(&self, path: &str) -> u64 {
        self.widest.get(path).copied().unwrap_or(0)
    }
}

/// What a string's bytes cost to copy or compare: one per this many.
const BYTES_PER_UNIT: u64 = 4;

/// What the walk knows of one node: its cost, the longest list and the
/// widest string or bytes value it can be or hold (at any depth), and its
/// path in the input when it is a selection from one.
#[derive(Clone)]
struct Info {
    cost: u64,
    len: u64,
    width: u64,
    known: Option<(String, Shape)>,
}

impl Info {
    fn scalar(cost: u64) -> Info {
        Info {
            cost,
            len: 0,
            width: 0,
            known: None,
        }
    }

    /// What touching every element and byte of this value costs.
    fn size(&self) -> u64 {
        self.len.saturating_add(self.width.div_ceil(BYTES_PER_UNIT))
    }
}

/// The cost bound of `expr` for `scope`, over `sizes`.
pub(crate) fn cost(expr: &IdedExpr, scope: Scope, sizes: &Sizes) -> u64 {
    let (lists, strings) = literal_sizes(expr);
    let mut walker = Walker {
        variables: scope
            .variables()
            .into_iter()
            .map(|(name, shape, path)| (name, (path.to_owned(), shape.clone())))
            .collect(),
        bound: Vec::new(),
        sizes,
        fallback_len: sizes.total.saturating_add(lists),
        fallback_width: sizes.widest_anywhere.saturating_add(strings),
        accumulators: Vec::new(),
    };
    walker.walk(expr).cost
}

/// The expression's literal list and map elements, and its literal
/// strings' bytes.
fn literal_sizes(expr: &IdedExpr) -> (u64, u64) {
    let (mut count, mut bytes) = (0u64, 0u64);
    let mut stack = vec![expr];
    while let Some(expr) = stack.pop() {
        match &expr.expr {
            Expr::List(list) => {
                count += list.elements.len() as u64;
                stack.extend(&list.elements);
            }
            Expr::Map(map) => {
                count += map.entries.len() as u64;
                for entry in &map.entries {
                    match &entry.expr {
                        EntryExpr::MapEntry(entry) => {
                            stack.push(&entry.key);
                            stack.push(&entry.value);
                        }
                        EntryExpr::StructField(field) => stack.push(&field.value),
                    }
                }
            }
            Expr::Struct(literal) => {
                for entry in &literal.entries {
                    if let EntryExpr::StructField(field) = &entry.expr {
                        stack.push(&field.value);
                    }
                }
            }
            Expr::Literal(literal) => bytes = bytes.saturating_add(literal_width(literal)),
            Expr::Select(select) => stack.push(&select.operand),
            Expr::Call(call) => {
                stack.extend(call.target.as_deref());
                stack.extend(&call.args);
            }
            Expr::Comprehension(c) => {
                stack.extend([
                    &c.iter_range,
                    &c.accu_init,
                    &c.loop_cond,
                    &c.loop_step,
                    &c.result,
                ]);
            }
            Expr::Unspecified | Expr::Ident(_) => {}
        }
    }
    (count, bytes)
}

fn literal_width(literal: &LiteralValue) -> u64 {
    match literal {
        LiteralValue::String(text) => text.len() as u64,
        LiteralValue::Bytes(bytes) => bytes.len() as u64,
        _ => 0,
    }
}

/// A comprehension variable: its path in the input, or what its range's
/// elements can be.
type Bound = Result<(String, Shape), Info>;

struct Walker<'a> {
    variables: Vec<(&'static str, (String, Shape))>,
    /// Comprehension variables in scope, innermost last: their path in the
    /// input when known, else what their range's elements can be.
    bound: Vec<(String, Bound)>,
    sizes: &'a Sizes,
    fallback_len: u64,
    fallback_width: u64,
    /// The comprehensions' accumulators being walked, innermost last, with
    /// what they can hold: `map` and `filter` append to one in place, so
    /// `@result + [x]` costs the element, not the list.
    accumulators: Vec<(String, Info)>,
}

impl Walker<'_> {
    /// A node at `path` with `shape`.
    fn at(&self, path: String, shape: Shape) -> Info {
        let (len, width) = match &shape {
            Shape::Scalar => (0, self.sizes.widest(&path)),
            Shape::Object(_) | Shape::List(_) | Shape::Map(_) | Shape::Any => {
                (self.sizes.longest(&path), self.sizes.widest(&path))
            }
        };
        Info {
            cost: 1,
            len,
            width,
            known: Some((path, shape)),
        }
    }

    /// A node nothing is known about.
    fn unknown(&self, cost: u64) -> Info {
        Info {
            cost,
            len: self.fallback_len,
            width: self.fallback_width,
            known: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn walk(&mut self, expr: &IdedExpr) -> Info {
        match &expr.expr {
            Expr::Unspecified => Info::scalar(1),
            Expr::Literal(literal) => Info {
                width: literal_width(literal),
                ..Info::scalar(1)
            },
            Expr::Ident(name) => {
                if let Some((_, info)) = self
                    .accumulators
                    .iter()
                    .rev()
                    .find(|(accu, _)| accu == name)
                {
                    return Info {
                        cost: 1,
                        ..info.clone()
                    };
                }
                if let Some((_, bound)) = self.bound.iter().rev().find(|(bound, _)| bound == name) {
                    return match bound.clone() {
                        Ok((path, shape)) => self.at(path, shape),
                        Err(info) => Info { cost: 1, ..info },
                    };
                }
                match self.variables.iter().find(|(var, _)| var == name) {
                    Some((_, (path, shape))) => self.at(path.clone(), shape.clone()),
                    // A type name, or one the static check refused.
                    None => Info::scalar(1),
                }
            }
            Expr::Select(select) => {
                let operand = self.walk(&select.operand);
                let cost = operand.cost.saturating_add(1);
                if select.test {
                    return Info::scalar(cost);
                }
                let known = operand.known.clone().and_then(|(path, shape)| match shape {
                    Shape::Object(fields) => fields
                        .get(&select.field)
                        .map(|shape| (format!("{path}.{}", select.field), shape.clone())),
                    Shape::Map(element) => Some((format!("{path}.*"), *element)),
                    Shape::Any => Some((format!("{path}.*"), Shape::Any)),
                    _ => None,
                });
                match known {
                    Some((path, shape)) => Info {
                        cost,
                        ..self.at(path, shape)
                    },
                    // A field of a value is no larger than the value.
                    None => Info {
                        cost,
                        known: None,
                        ..operand
                    },
                }
            }
            Expr::List(list) => {
                let elements: Vec<Info> = list.elements.iter().map(|e| self.walk(e)).collect();
                Info {
                    cost: elements
                        .iter()
                        .map(|e| e.cost)
                        .fold(1u64, u64::saturating_add),
                    len: elements
                        .iter()
                        .map(|e| e.len)
                        .fold(list.elements.len() as u64, u64::max),
                    width: elements.iter().map(|e| e.width).fold(0, u64::max),
                    known: None,
                }
            }
            Expr::Map(map) => {
                let mut info = Info {
                    len: map.entries.len() as u64,
                    ..Info::scalar(1)
                };
                for entry in &map.entries {
                    if let EntryExpr::MapEntry(entry) = &entry.expr {
                        for part in [self.walk(&entry.key), self.walk(&entry.value)] {
                            info.cost = info.cost.saturating_add(part.cost);
                            info.len = info.len.max(part.len);
                            info.width = info.width.max(part.width);
                        }
                    }
                }
                info
            }
            Expr::Struct(literal) => {
                let mut cost = 1u64;
                for entry in &literal.entries {
                    if let EntryExpr::StructField(field) = &entry.expr {
                        cost = cost.saturating_add(self.walk(&field.value).cost);
                    }
                }
                self.unknown(cost)
            }
            Expr::Call(call) => self.call(call),
            Expr::Comprehension(c) => {
                let range = self.walk(&c.iter_range);
                let init = self.walk(&c.accu_init);
                let element = match &range.known {
                    Some((path, shape)) => Ok((
                        format!("{path}.*"),
                        shape.element().cloned().unwrap_or(Shape::Any),
                    )),
                    None => Err(Info {
                        cost: 1,
                        known: None,
                        ..range.clone()
                    }),
                };
                let builds = matches!(c.accu_init.expr, Expr::List(_));
                let depth = self.bound.len();
                match &c.iter_var2 {
                    Some(second) => {
                        self.bound.push((c.iter_var.clone(), Err(Info::scalar(1))));
                        self.bound.push((second.clone(), element));
                    }
                    None => self.bound.push((c.iter_var.clone(), element)),
                }
                let accumulator = Info {
                    len: if builds { range.len } else { 0 },
                    ..init.clone()
                };
                self.accumulators.push((c.accu_var.clone(), accumulator));
                let cond = self.walk(&c.loop_cond);
                let step = self.walk(&c.loop_step);
                self.bound.truncate(depth);
                let held = Info {
                    len: if builds { range.len.max(step.len) } else { 0 },
                    width: if builds { step.width } else { 0 },
                    ..Info::scalar(1)
                };
                if let Some(last) = self.accumulators.last_mut() {
                    last.1 = held.clone();
                }
                let result = self.walk(&c.result);
                self.accumulators.pop();
                let per = cond.cost.saturating_add(step.cost);
                Info {
                    cost: range
                        .cost
                        .saturating_add(init.cost)
                        .saturating_add(result.cost)
                        .saturating_add(range.len.saturating_mul(per)),
                    len: if builds { held.len } else { result.len },
                    width: if builds { held.width } else { result.width },
                    known: None,
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn call(&mut self, call: &cel::common::ast::CallExpr) -> Info {
        let name = call.func_name.as_str();
        let target = call.target.as_deref().map(|target| self.walk(target));
        let args: Vec<Info> = call.args.iter().map(|arg| self.walk(arg)).collect();
        let cost = target
            .iter()
            .chain(&args)
            .map(|info| info.cost)
            .fold(1u64, u64::saturating_add);
        // The receiver of a method call is its first operand.
        let operands: Vec<&Info> = target.iter().chain(&args).collect();
        let first = operands.first().copied();
        let all_sizes = operands
            .iter()
            .map(|info| info.size())
            .fold(0u64, u64::saturating_add);
        let scalar = |charge: u64| Info::scalar(cost.saturating_add(charge));
        let appends = name == "_+_"
            && call.args.first().is_some_and(|arg| {
                matches!(&arg.expr, Expr::Ident(accu) if self.accumulators.iter().any(|(a, _)| a == accu))
            });
        match name {
            // `map`'s and `filter`'s in-place append: the element, not the list.
            "_+_" if appends => {
                let accu = &args[0];
                let element = args.get(1).cloned().unwrap_or_else(|| Info::scalar(0));
                Info {
                    cost: cost.saturating_add(element.size()),
                    len: accu.len.max(element.len),
                    width: accu.width.max(element.width),
                    known: None,
                }
            }
            // Concatenation copies both sides.
            "_+_" => {
                let len = operands
                    .iter()
                    .map(|i| i.len)
                    .fold(0u64, u64::saturating_add);
                let width = operands
                    .iter()
                    .map(|i| i.width)
                    .fold(0u64, u64::saturating_add);
                let result = Info {
                    cost: 0,
                    len,
                    width,
                    known: None,
                };
                Info {
                    cost: cost.saturating_add(result.size()),
                    ..result
                }
            }
            // Comparisons and searches read both sides.
            "_==_" | "_!=_" | "_<_" | "_<=_" | "_>_" | "_>=_" | "@in" | "contains"
            | "startsWith" | "endsWith" | "reef_matches" | "glob" | "any_glob" | "all_glob" => {
                scalar(all_sizes)
            }
            "matching" => {
                let paths = first.cloned().unwrap_or_else(|| Info::scalar(0));
                Info {
                    cost: cost.saturating_add(all_sizes),
                    known: None,
                    ..paths
                }
            }
            "_?_:_" => {
                let (len, width) = args
                    .iter()
                    .skip(1)
                    .fold((0, 0), |(l, w), i| (l.max(i.len), w.max(i.width)));
                Info {
                    cost,
                    len,
                    width,
                    known: None,
                }
            }
            "_[_]" | "_[?_]" | "_?._" => match args.first().and_then(|arg| arg.known.clone()) {
                Some((path, Shape::List(element) | Shape::Map(element))) => Info {
                    cost,
                    ..self.at(format!("{path}.*"), *element)
                },
                Some((path, Shape::Any)) => Info {
                    cost,
                    ..self.at(format!("{path}.*"), Shape::Any)
                },
                _ => {
                    let from = args.first().cloned().unwrap_or_else(|| self.unknown(0));
                    Info {
                        cost,
                        known: None,
                        ..from
                    }
                }
            },
            // Booleans and numbers.
            "size"
            | "int"
            | "uint"
            | "double"
            | "bool"
            | "type"
            | "hasValue"
            | "!_"
            | "-_"
            | "_&&_"
            | "_||_"
            | "@not_strictly_false"
            | "_-_"
            | "_*_"
            | "_/_"
            | "_%_" => scalar(0),
            "string" => Info {
                width: first.map_or(0, |i| i.width.max(i.len)).max(32),
                ..scalar(first.map_or(0, Info::size))
            },
            "bytes" => Info {
                width: first.map_or(0, |i| i.width),
                ..scalar(first.map_or(0, Info::size))
            },
            // The same value, or one of its arguments.
            "dyn" | "value" | "or" | "orValue" | "of" | "ofNonZeroValue" => {
                let (len, width) = operands
                    .iter()
                    .fold((0, 0), |(l, w), i| (l.max(i.len), w.max(i.width)));
                Info {
                    cost,
                    len,
                    width,
                    known: None,
                }
            }
            _ => self.unknown(cost),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::shapes;
    use crate::env::parse;

    fn sizes(paths: usize) -> (Sizes, Json) {
        let change = serde_json::json!({
            "paths": (0..paths).map(|i| format!("p{i}")).collect::<Vec<_>>(),
            "files": [],
        });
        let evidence = serde_json::json!([
            {"artifacts": [{}, {}, {}]},
            {"artifacts": [{}]},
        ]);
        let shapes = shapes();
        let sizes = Sizes::of(&[
            ("change", &change, &shapes.change),
            ("evidence", &evidence, &shapes.evidence),
        ]);
        (sizes, change)
    }

    fn of(source: &str, scope: Scope, sizes: &Sizes) -> u64 {
        cost(&parse(source).unwrap().ast, scope, sizes)
    }

    #[test]
    fn known_lists_are_bounded_by_this_input() {
        let (sizes, _) = sizes(100);
        assert_eq!(sizes.longest("change.paths"), 100);
        assert_eq!(sizes.longest("evidence.*.artifacts"), 3);
        let one = of("change.paths.all(p, p != 'x')", Scope::Change, &sizes);
        assert!((400..=1500).contains(&one), "{one}");
        let nested = of(
            "change.paths.all(a, change.paths.exists(b, a == b))",
            Scope::Change,
            &sizes,
        );
        assert!(nested > 100 * 100, "{nested}");
        let artifacts = of(
            "e.artifacts.exists(a, a.state == 'stored')",
            Scope::Evidence,
            &sizes,
        );
        assert!(artifacts < 40, "{artifacts}");
    }

    #[test]
    fn the_nested_loop_over_many_paths_is_over_the_bound() {
        let (sizes, _) = sizes(100_000);
        let started = std::time::Instant::now();
        let nested = of(
            "change.paths.all(a, change.paths.exists(b, a == b))",
            Scope::Change,
            &sizes,
        );
        assert!(nested > MAX_EXPRESSION_COST, "{nested}");
        assert!(started.elapsed() < std::time::Duration::from_millis(10));
        let flat = of("any_glob(change.paths, ['docs/**'])", Scope::Change, &sizes);
        assert!(flat < 100_100, "{flat}");
        // A filter over every path is linear: it appends in place.
        let filter = of(
            "change.paths.filter(p, p.startsWith('a')).size() < 5",
            Scope::Change,
            &sizes,
        );
        assert!(filter < 2_000_000, "{filter}");
    }

    #[test]
    fn literals_maps_filters_and_concatenation_are_bounded() {
        let (sizes, _) = sizes(10);
        let literal = of("[1, 2, 3].all(x, x > 0)", Scope::Change, &sizes);
        let filter = of(
            "change.paths.filter(p, p != '').all(q, q != '')",
            Scope::Change,
            &sizes,
        );
        let concat = of(
            "(change.paths + change.paths).all(q, q != '')",
            Scope::Change,
            &sizes,
        );
        let matching = of(
            "matching(change.paths, ['*']).all(q, q != '')",
            Scope::Change,
            &sizes,
        );
        assert!(literal < 40, "{literal}");
        assert!(filter < 500, "{filter}");
        assert!((100..600).contains(&concat), "{concat}");
        assert!(matching < 400, "{matching}");
        // An unknown list (a call's result) falls back to the input's total.
        let unknown = of("dyn(change.paths).all(q, q != '')", Scope::Change, &sizes);
        assert!(unknown >= 10, "{unknown}");
    }

    #[test]
    fn values_that_double_are_charged_their_size() {
        // Review finding 1: `a + a` doubles a string, and `map` feeds it on.
        let (sizes, _) = sizes(10);
        for (start, step) in [
            ("['abcdefgh']", ".map(a, a + a)"),
            ("[[1]]", ".map(l, l + l)"),
        ] {
            let source = format!("{start}{}.size() > 0", step.repeat(30));
            let cost = of(&source, Scope::Change, &sizes);
            assert!(cost > MAX_EXPRESSION_COST, "{source}: {cost}");
            let small = format!("{start}{}.size() > 0", step.repeat(4));
            assert!(of(&small, Scope::Change, &sizes) < 10_000);
        }
        // Long strings cost their bytes to compare and search.
        let wide = format!("'{}'.contains('x') && true", "y".repeat(400));
        assert!(of(&wide, Scope::Change, &sizes) >= 100);
    }

    #[test]
    fn scalars_from_calls_are_not_charged_as_lists() {
        // Review finding 4: `p.size() != 0` is two numbers.
        let (sizes, _) = sizes(5_000);
        let cost = of("change.paths.all(p, p.size() != 0)", Scope::Change, &sizes);
        assert!(cost < 200_000, "{cost}");
    }

    #[test]
    fn the_bound_is_the_same_on_every_run() {
        let (sizes, _) = sizes(1_000);
        let source =
            "change.paths.exists(p, glob(p, 'a/**')) && evidence.all(e, e.artifacts.size() > 0)";
        let first = of(source, Scope::Change, &sizes);
        for _ in 0..10 {
            assert_eq!(of(source, Scope::Change, &sizes), first);
        }
    }
}
