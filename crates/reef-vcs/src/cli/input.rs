//! Fields from one JSON object on stdin (R2.26's provider contract, ADR
//! 0053; R3.44's `provenance declare`): `--stdin` reads the object, keyed
//! by the flags' names, and `--at FIELD=POINTER` takes a field from
//! anywhere in it (RFC 6901), so a program's own event maps onto a command
//! without code. A flag wins over a pointer, a pointer over the object's
//! own key, and a key over the default.

use std::io::{self, Read as _};

use serde_json::Value;

use super::CliError;

/// The most stdin is read.
pub(crate) const STDIN_MAX: u64 = 1024 * 1024;

/// Where the fields come from besides flags.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct Input {
    /// Read the fields as one JSON object on stdin, keyed by the flags' names (`new_on`).
    #[arg(long)]
    pub(crate) stdin: bool,
    /// Take FIELD from stdin's object at a JSON pointer, such as `repo=/cwd` (implies --stdin).
    #[arg(long = "at", value_name = "FIELD=POINTER")]
    pub(crate) at: Vec<String>,
}

impl Input {
    /// Whether stdin is to be read.
    pub(crate) fn reads_stdin(&self) -> bool {
        self.stdin || !self.at.is_empty()
    }
}

/// What a command's fields are and how it reports a bad one.
pub(crate) struct Rules {
    /// Every field the command takes.
    pub(crate) known: &'static [&'static str],
    /// Fields that may also be whole numbers (a pid).
    pub(crate) numbers: &'static [&'static str],
    /// The command's error for `field`.
    pub(crate) invalid: fn(&'static str, String) -> CliError,
}

/// The stdin object and the `--at` pointers into it. Its `Debug` leaves
/// the object out: it may hold a prompt's text.
pub(crate) struct Fields {
    stdin: Option<Value>,
    at: Vec<(&'static str, String)>,
    numbers: &'static [&'static str],
    invalid: fn(&'static str, String) -> CliError,
}

impl std::fmt::Debug for Fields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fields")
            .field("stdin", &self.stdin.as_ref().map(|_| "<redacted>"))
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl Fields {
    /// Reads stdin if `--stdin` or `--at` asks for it, and checks the
    /// pointers' fields.
    pub(crate) fn read(input: &Input, rules: &Rules) -> Result<Self, CliError> {
        let stdin = if input.reads_stdin() {
            let mut bytes = Vec::new();
            io::stdin()
                .take(STDIN_MAX + 1)
                .read_to_end(&mut bytes)
                .map_err(|source| CliError::Io {
                    op: "read",
                    path: "stdin".into(),
                    source,
                })?;
            Some(parse_stdin(&bytes, rules)?)
        } else {
            None
        };
        Self::with(stdin, &input.at, rules)
    }

    /// The fields of `stdin`, already read.
    pub(crate) fn with(
        stdin: Option<Value>,
        at: &[String],
        rules: &Rules,
    ) -> Result<Self, CliError> {
        let invalid = rules.invalid;
        let at = at
            .iter()
            .map(|pair| {
                let (field, pointer) = pair.split_once('=').unwrap_or((pair, ""));
                let field = rules
                    .known
                    .iter()
                    .find(|known| **known == field)
                    .ok_or_else(|| {
                        invalid(
                            "at",
                            format!(
                                "--at {pair}: no field `{field}`; this command takes {}",
                                rules.known.join(", ")
                            ),
                        )
                    })?;
                if !(pointer.is_empty() || pointer.starts_with('/')) {
                    return Err(invalid(
                        "at",
                        format!("--at {pair}: a JSON pointer is empty or starts with `/`"),
                    ));
                }
                Ok((*field, pointer.to_owned()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            stdin,
            at,
            numbers: rules.numbers,
            invalid,
        })
    }

    /// `field`'s value: the flag, else the pointer's, else the object's
    /// key; `None` when none gives one.
    pub(crate) fn get(
        &self,
        field: &'static str,
        flag: Option<String>,
    ) -> Result<Option<String>, CliError> {
        if flag.is_some() {
            return Ok(flag);
        }
        let Some(stdin) = &self.stdin else {
            return Ok(None);
        };
        for (_, pointer) in self.at.iter().filter(|(name, _)| *name == field) {
            if let Some(value) = self.text(field, stdin.pointer(pointer))? {
                return Ok(Some(value));
            }
        }
        self.text(field, stdin.get(field))
    }

    /// A field's value as text: a string, or for a number field a whole
    /// number; `null` and absent are none.
    fn text(&self, field: &'static str, value: Option<&Value>) -> Result<Option<String>, CliError> {
        let number = self.numbers.contains(&field);
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(Value::Number(value)) if number && value.is_u64() => Ok(Some(value.to_string())),
            Some(_) => Err((self.invalid)(
                field,
                format!(
                    "`{field}` must be a string{}",
                    if number { " or a pid" } else { "" }
                ),
            )),
        }
    }
}

/// Stdin as one JSON object of at most [`STDIN_MAX`] bytes.
pub(crate) fn parse_stdin(bytes: &[u8], rules: &Rules) -> Result<Value, CliError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > STDIN_MAX {
        return Err((rules.invalid)("stdin", "stdin is over 1 MiB".to_owned()));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| (rules.invalid)("stdin", format!("stdin is not JSON: {error}")))?;
    if !value.is_object() {
        return Err((rules.invalid)(
            "stdin",
            "stdin is not one JSON object".to_owned(),
        ));
    }
    Ok(value)
}
