//! The CEL environment a policy's expressions run in: cel-rust's standard
//! library and macros, with optional syntax and no extensions, plus Reef's
//! four path functions.
//!
//! This module, [`crate::check`] and [`crate::cost`] are the only users of
//! cel-rust's API, so a breaking release of it touches three modules.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use cel::common::ast::SourceInfo;
use cel::{Context, Env, ExecutionError, IdedExpr, Value};

use crate::glob::Patterns;

/// The parser's recursion depth: deeper nesting is refused while parsing.
pub const MAX_DEPTH: u16 = 48;

/// Reef's functions, all pure, each called with two arguments and no
/// target.
pub const REEF_FUNCTIONS: [&str; 4] = ["glob", "any_glob", "all_glob", "matching"];

/// The standard functions this environment has, other than operators and
/// macros: what `Env::stdlib` registers in cel 0.15.0 without `chrono`
/// (a test checks each against the environment).
pub const STANDARD_FUNCTIONS: [&str; 20] = [
    "bool",
    "bytes",
    "contains",
    "double",
    "dyn",
    "endsWith",
    "hasValue",
    "int",
    "matches",
    "optional.none",
    "optional.of",
    "optional.ofNonZeroValue",
    "or",
    "orValue",
    "size",
    "startsWith",
    "string",
    "type",
    "uint",
    "value",
];

/// Identifiers that name types, usable as values (`type(x) == string`).
pub const TYPE_NAMES: [&str; 11] = [
    "bool",
    "bytes",
    "double",
    "dyn",
    "int",
    "list",
    "map",
    "null_type",
    "string",
    "type",
    "uint",
];

/// The shared environment.
pub(crate) fn env() -> &'static Arc<Env> {
    static ENV: OnceLock<Arc<Env>> = OnceLock::new();
    ENV.get_or_init(|| Arc::new(Env::stdlib()))
}

/// A parsed expression, with where each node was written in its source.
#[derive(Debug)]
pub(crate) struct Parsed {
    pub(crate) ast: IdedExpr,
    pub(crate) info: SourceInfo,
}

impl Parsed {
    /// The byte offset in the expression's source where node `id` starts.
    pub(crate) fn offset(&self, id: u64) -> usize {
        self.info
            .offset_for(id)
            .map_or(0, |(start, _)| start as usize)
    }
}

/// The stack a load, an evaluation or a conflict lookup runs on: cel-rust's
/// parser recurses (through about a dozen grammar rules per nesting level)
/// before its own depth limit applies, and its evaluator, the static check
/// and the cost bound recurse over the AST, so a policy runs on a thread of
/// its own whatever thread called it.
pub const STACK_BYTES: usize = 64 << 20;

/// Runs `f` on a thread with [`STACK_BYTES`] of stack; `None` when no
/// thread could be started.
pub(crate) fn on_policy_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Option<T> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("reef-policy".to_owned())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, f)
            .ok()?;
        match handle.join() {
            Ok(value) => Some(value),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// An upper bound on how deep `source` nests, read from its text before
/// the parser sees it: the deepest bracket nesting, plus the longest run of
/// unary operators, plus every `?` that is not optional syntax (`.?`,
/// `[?`). Strings and `//` comments are skipped.
pub(crate) fn nesting(source: &str) -> usize {
    let bytes = source.as_bytes();
    let (mut depth, mut deepest, mut run, mut longest, mut ternaries) = (0usize, 0, 0usize, 0, 0);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'\'' || c == b'"' {
            let raw = i > 0 && matches!(bytes[i - 1], b'r' | b'R');
            let triple = bytes.get(i + 1) == Some(&c) && bytes.get(i + 2) == Some(&c);
            i += if triple { 3 } else { 1 };
            while i < bytes.len() {
                if !raw && bytes[i] == b'\\' {
                    i += 2;
                } else if triple && bytes[i..].starts_with(&[c, c, c]) {
                    i += 3;
                    break;
                } else if !triple && bytes[i] == c {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            run = 0;
            continue;
        }
        match c {
            b'(' | b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
                run = 0;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                run = 0;
            }
            b'!' if bytes.get(i + 1) == Some(&b'=') => {
                i += 1;
                run = 0;
            }
            b'!' | b'-' => {
                run += 1;
                longest = longest.max(run);
            }
            b'?' if i > 0 && matches!(bytes[i - 1], b'.' | b'[') => {}
            b'?' => ternaries += 1,
            c if c.is_ascii_whitespace() => {}
            _ => run = 0,
        }
        i += 1;
    }
    deepest + longest + ternaries
}

/// `source` parsed at [`MAX_DEPTH`], or each error with its byte offset in
/// `source`.
pub(crate) fn parse(source: &str) -> Result<Parsed, Vec<(usize, String)>> {
    let nested = nesting(source);
    if nested > usize::from(MAX_DEPTH) {
        return Err(vec![(
            0,
            format!(
                "the expression nests {nested} deep (brackets, unary operators and `?`); at most {MAX_DEPTH}"
            ),
        )]);
    }
    match env()
        .parser()
        .max_recursion_depth(MAX_DEPTH)
        .parse_with_source_info(source)
    {
        Ok((ast, info)) => Ok(Parsed { ast, info }),
        Err(errors) => Err(errors
            .errors
            .iter()
            .map(|error| (offset_of(source, error.pos), error.msg.clone()))
            .collect()),
    }
}

/// The byte offset of a parse error's 1-based line and 0-based column.
fn offset_of(source: &str, (line, column): (isize, isize)) -> usize {
    let line = usize::try_from(line.max(1)).unwrap_or(1);
    let column = usize::try_from(column.max(0)).unwrap_or(0);
    let start: usize = source
        .split_inclusive('\n')
        .take(line - 1)
        .map(str::len)
        .sum();
    (start + column).min(source.len())
}

/// The most distinct pattern lists, and regular expressions, one
/// evaluation compiles: past it, a call is an error.
pub const MAX_COMPILED: usize = 64;

/// The most patterns in one list given to Reef's glob functions.
pub const MAX_PATTERNS: usize = 256;

/// The most memory one compiled regular expression may take, in bytes.
pub const MAX_REGEX_BYTES: usize = 256 * 1024;

/// A regular expression as `matches` compiles it: a linear-time engine,
/// with [`MAX_REGEX_BYTES`] of program at most.
pub(crate) fn regex(pattern: &str) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .size_limit(MAX_REGEX_BYTES)
        .dfa_size_limit(MAX_REGEX_BYTES)
        .build()
        .map_err(|error| error.to_string())
}

/// Rewrites every `matches` call, global or on a target, into Reef's
/// `reef_matches(text, pattern)`, which compiles each pattern once per
/// evaluation and with a size limit (cel-rust's compiles on every call,
/// without one). Run after the static check, which knows `matches`.
pub(crate) fn rewrite_matches(expr: &mut IdedExpr) {
    use cel::common::ast::{EntryExpr, Expr};
    match &mut expr.expr {
        Expr::Call(call) => {
            if let Some(target) = call.target.as_deref_mut() {
                rewrite_matches(target);
            }
            for arg in &mut call.args {
                rewrite_matches(arg);
            }
            if call.func_name == "matches" {
                if let Some(target) = call.target.take() {
                    call.args.insert(0, *target);
                }
                "reef_matches".clone_into(&mut call.func_name);
            }
        }
        Expr::Select(select) => rewrite_matches(&mut select.operand),
        Expr::List(list) => list.elements.iter_mut().for_each(rewrite_matches),
        Expr::Map(map) => {
            for entry in &mut map.entries {
                if let EntryExpr::MapEntry(entry) = &mut entry.expr {
                    rewrite_matches(&mut entry.key);
                    rewrite_matches(&mut entry.value);
                }
            }
        }
        Expr::Struct(literal) => {
            for entry in &mut literal.entries {
                if let EntryExpr::StructField(field) = &mut entry.expr {
                    rewrite_matches(&mut field.value);
                }
            }
        }
        Expr::Comprehension(c) => {
            for part in [
                &mut c.iter_range,
                &mut c.accu_init,
                &mut c.loop_cond,
                &mut c.loop_step,
                &mut c.result,
            ] {
                rewrite_matches(part);
            }
        }
        Expr::Unspecified | Expr::Ident(_) | Expr::Literal(_) => {}
    }
}

/// Compiled patterns by their source, per evaluation: a pattern written in
/// an expression is compiled once however often it is called, and at most
/// [`MAX_COMPILED`] are.
type Compiled = Result<Arc<Patterns>, String>;

#[derive(Clone, Default)]
struct GlobCache(Arc<Mutex<HashMap<Vec<String>, Compiled>>>);

impl GlobCache {
    fn get(&self, patterns: Vec<String>) -> Result<Arc<Patterns>, String> {
        if patterns.len() > MAX_PATTERNS {
            return Err(format!(
                "{} patterns; a list has at most {MAX_PATTERNS}",
                patterns.len()
            ));
        }
        let mut cache = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cache.contains_key(&patterns) && cache.len() >= MAX_COMPILED {
            return Err(format!(
                "more than {MAX_COMPILED} distinct pattern lists in one evaluation"
            ));
        }
        cache
            .entry(patterns)
            .or_insert_with_key(|patterns| {
                Patterns::new(patterns.iter().map(String::as_str)).map(Arc::new)
            })
            .clone()
    }
}

fn strings(function: &str, what: &str, list: &[Value]) -> Result<Vec<String>, ExecutionError> {
    list.iter()
        .map(|value| match value {
            Value::String(text) => Ok(text.as_str().to_owned()),
            other => Err(ExecutionError::function_error(
                function,
                format!("{what} must be strings, not {:?}", other.type_of()),
            )),
        })
        .collect()
}

fn patterns(
    cache: &GlobCache,
    function: &str,
    list: &[Value],
) -> Result<Arc<Patterns>, ExecutionError> {
    cache
        .get(strings(function, "patterns", list)?)
        .map_err(|error| ExecutionError::function_error(function, error))
}

/// A context with `variables` bound and Reef's functions registered.
pub(crate) fn context(variables: Vec<(&'static str, Value)>) -> Context<'static, 'static> {
    let mut context = Context::with_env(Arc::clone(env()));
    for (name, value) in variables {
        context.add_variable_from_value(name, value);
    }
    let cache = GlobCache::default();
    let glob = cache.clone();
    let regexes: Arc<Mutex<HashMap<String, Result<regex::Regex, String>>>> = Arc::default();
    let registered = [
        context.add_function(
            "glob",
            move |path: Arc<String>, pattern: Arc<String>| -> Result<bool, ExecutionError> {
                let compiled = glob
                    .get(vec![pattern.as_str().to_owned()])
                    .map_err(|error| ExecutionError::function_error("glob", error))?;
                Ok(compiled.matches(&path))
            },
        ),
        {
            let cache = cache.clone();
            context.add_function(
                "any_glob",
                move |paths: Arc<Vec<Value>>,
                      list: Arc<Vec<Value>>|
                      -> Result<bool, ExecutionError> {
                    let compiled = patterns(&cache, "any_glob", &list)?;
                    let paths = strings("any_glob", "paths", &paths)?;
                    Ok(paths.iter().any(|path| compiled.matches(path)))
                },
            )
        },
        {
            let cache = cache.clone();
            context.add_function(
                "all_glob",
                move |paths: Arc<Vec<Value>>,
                      list: Arc<Vec<Value>>|
                      -> Result<bool, ExecutionError> {
                    let compiled = patterns(&cache, "all_glob", &list)?;
                    let paths = strings("all_glob", "paths", &paths)?;
                    Ok(paths.iter().all(|path| compiled.matches(path)))
                },
            )
        },
        context.add_function(
            "matching",
            move |paths: Arc<Vec<Value>>,
                  list: Arc<Vec<Value>>|
                  -> Result<Arc<Vec<Value>>, ExecutionError> {
                let compiled = patterns(&cache, "matching", &list)?;
                let paths = strings("matching", "paths", &paths)?;
                Ok(Arc::new(
                    paths
                        .into_iter()
                        .filter(|path| compiled.matches(path))
                        .map(|path| Value::String(Arc::new(path)))
                        .collect(),
                ))
            },
        ),
    ];
    let matches = context.add_function(
        "reef_matches",
        move |text: Arc<String>, pattern: Arc<String>| -> Result<bool, ExecutionError> {
            let mut cache = regexes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !cache.contains_key(pattern.as_str()) && cache.len() >= MAX_COMPILED {
                return Err(ExecutionError::function_error(
                    "matches",
                    format!("more than {MAX_COMPILED} distinct patterns in one evaluation"),
                ));
            }
            let compiled = cache
                .entry(pattern.as_str().to_owned())
                .or_insert_with_key(|pattern| regex(pattern));
            match compiled {
                Ok(compiled) => Ok(compiled.is_match(&text)),
                Err(error) => Err(ExecutionError::function_error("matches", error.clone())),
            }
        },
    );
    for result in registered.into_iter().chain([matches]) {
        // Reef's names are not the standard library's: a test checks it.
        debug_assert!(result.is_ok(), "{result:?}");
    }
    context
}

/// What an expression evaluated to: a boolean, or why not.
pub(crate) fn boolean(context: &Context<'_, '_>, parsed: &Parsed) -> Result<bool, String> {
    match context.resolve(&parsed.ast) {
        Ok(Value::Bool(value)) => Ok(value),
        Ok(other) => Err(format!(
            "the expression is a {:?}, not a bool",
            other.type_of()
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// `value` as a CEL value: the input's serde form.
pub(crate) fn to_value<T: serde::Serialize>(value: &T) -> Value {
    // The input's types are plain structs, lists, strings, integers and
    // booleans, and maps with string keys: none fails to convert.
    cel::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_functions_are_the_environments() {
        for name in STANDARD_FUNCTIONS {
            let mut context = Context::with_env(Arc::clone(env()));
            assert!(
                context.add_function(name, |a: i64| a).is_err(),
                "{name} is not a standard function"
            );
        }
        for name in REEF_FUNCTIONS {
            let mut context = Context::with_env(Arc::clone(env()));
            assert!(context.add_function(name, |a: i64| a).is_ok(), "{name}");
        }
    }

    #[test]
    fn reef_functions_match_paths() {
        let context = context(vec![("paths", to_value(&vec!["crates/a.rs", "docs/b.md"]))]);
        for (source, expected) in [
            ("glob('crates/a.rs', 'crates/*')", true),
            ("glob('crates/x/a.rs', 'crates/*')", false),
            ("any_glob(paths, ['docs/**'])", true),
            ("all_glob(paths, ['docs/**'])", false),
            ("matching(paths, ['**/*.md']) == ['docs/b.md']", true),
            ("glob('Secrets/k', 'secrets/**')", true),
        ] {
            let parsed = parse(source).unwrap();
            assert_eq!(boolean(&context, &parsed), Ok(expected), "{source}");
        }
        let parsed = parse("glob('a', 'a/[')").unwrap();
        assert!(boolean(&context, &parsed).is_err());
        let parsed = parse("any_glob([1], ['a'])").unwrap();
        assert!(boolean(&context, &parsed).is_err());
    }

    #[test]
    fn nesting_is_read_from_the_text() {
        assert_eq!(nesting("a"), 0);
        assert_eq!(nesting("((a))"), 2);
        assert_eq!(nesting("!!!a"), 3);
        assert_eq!(nesting("a != b"), 0);
        assert_eq!(nesting("a ? b : c ? d : e"), 2);
        assert_eq!(nesting("'((((' + \"[[\\\"[[\" + '''(((('''"), 0);
        assert_eq!(nesting("f(a, [1, (2)])"), 3);
        // Review finding 10: a comment hides nothing, and optional syntax
        // is not a ternary.
        assert_eq!(nesting("a // it's\n((((b))))"), 4);
        assert_eq!(nesting("a.?b.?c[?0]"), 1);
    }

    #[test]
    fn deep_nesting_is_refused_before_the_parser_sees_it() {
        for source in [
            format!("{}a", "!".repeat(5_000)),
            format!("{}1{}", "(".repeat(5_000), ")".repeat(5_000)),
            format!("{}a", "-".repeat(5_000)),
            vec!["a ? b : c"; 2_000].join(" ? "),
        ] {
            let errors = parse(&source).unwrap_err();
            assert!(errors[0].1.contains("nests"), "{errors:?}");
        }
    }

    #[test]
    fn the_deepest_allowed_expression_parses_on_the_policy_stack() {
        let source = format!(
            "{}1{}",
            "(".repeat(usize::from(MAX_DEPTH)),
            ")".repeat(usize::from(MAX_DEPTH))
        );
        assert!(on_policy_stack(|| parse(&source).is_ok()).unwrap());
    }

    #[test]
    fn parse_errors_carry_an_offset() {
        let errors = parse("a &&\n  )").unwrap_err();
        assert_ne!(errors, [] as [(usize, String); 0]);
        assert!(errors[0].0 >= 5, "{errors:?}");
        let deep = format!("{}1{}", "(".repeat(60), ")".repeat(60));
        assert!(parse(&deep).is_err());
    }
}
