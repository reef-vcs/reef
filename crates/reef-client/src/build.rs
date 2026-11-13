//! Applying a build environment (#271, ADR 0074): what `reef ws run`, the
//! provider contract's `env` and `reef ws env` set from a
//! `GetBuildEnvironmentResponse`.

use std::ffi::OsString;

use reef_api::v1::{BuildVariableMerge, GetBuildEnvironmentResponse};

/// The variables to set, in order, for a command whose environment
/// `lookup` reads: each one the environment lacks, `GOFLAGS` merged, and
/// no jobserver for a command already under a `make`'s.
pub fn assemble(
    response: &GetBuildEnvironmentResponse,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Vec<(String, String)> {
    if !response.enabled {
        return Vec::new();
    }
    let under_make = ["MAKEFLAGS", "MFLAGS"].iter().any(|name| {
        lookup(name).is_some_and(|value| {
            let value = value.to_string_lossy();
            value.contains("--jobserver-auth") || value.contains("--jobserver-fds")
        })
    });
    let mut out = Vec::new();
    for variable in &response.variables {
        let present = lookup(&variable.name);
        let merge = BuildVariableMerge::try_from(variable.merge).unwrap_or_default();
        match merge {
            BuildVariableMerge::Goflags => match present {
                Some(flags) if !flags.is_empty() => {
                    let flags = flags.to_string_lossy();
                    if !has_p_flag(&flags) {
                        out.push((
                            variable.name.clone(),
                            format!("{} -p={}", flags.trim(), response.share),
                        ));
                    }
                }
                _ if !variable.value.is_empty() => {
                    out.push((variable.name.clone(), variable.value.clone()));
                }
                _ => {}
            },
            BuildVariableMerge::IfUnset | BuildVariableMerge::Unspecified => {
                if present.is_some() {
                    continue;
                }
                if under_make && matches!(variable.name.as_str(), "CARGO_MAKEFLAGS" | "MAKEFLAGS") {
                    continue;
                }
                out.push((variable.name.clone(), variable.value.clone()));
            }
        }
    }
    out
}

/// Whether `flags` (a `GOFLAGS` value) has a `-p` flag.
fn has_p_flag(flags: &str) -> bool {
    flags.split_whitespace().any(|flag| {
        let flag = flag.trim_start_matches('-');
        flag == "p" || flag.starts_with("p=")
    })
}

/// `name='value'` quoted for a POSIX shell's `export`.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use reef_api::v1::BuildVariable;

    use super::*;

    fn response() -> GetBuildEnvironmentResponse {
        let variable = |name: &str, value: &str, merge: BuildVariableMerge| BuildVariable {
            name: name.to_owned(),
            value: value.to_owned(),
            merge: merge.into(),
        };
        GetBuildEnvironmentResponse {
            enabled: true,
            share: 2,
            variables: vec![
                variable(
                    "CARGO_MAKEFLAGS",
                    "-j --jobserver-auth=fifo:/s/build/jobserver",
                    BuildVariableMerge::IfUnset,
                ),
                variable("RUST_TEST_THREADS", "2", BuildVariableMerge::IfUnset),
                variable("GOFLAGS", "-trimpath -p=2", BuildVariableMerge::Goflags),
            ],
            ..GetBuildEnvironmentResponse::default()
        }
    }

    fn with(env: &[(&str, &str)]) -> Vec<(String, String)> {
        let env: HashMap<String, OsString> = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        assemble(&response(), |name| env.get(name).cloned())
    }

    #[test]
    fn sets_what_is_missing() {
        let all = with(&[]);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2], ("GOFLAGS".to_owned(), "-trimpath -p=2".to_owned()));
        // A set variable wins, even an empty one (the per-command way out).
        let kept = with(&[("RUST_TEST_THREADS", "7"), ("CARGO_MAKEFLAGS", "")]);
        assert_eq!(kept.len(), 1);
        // GOFLAGS is merged, or left alone with a -p.
        assert_eq!(with(&[("GOFLAGS", "-mod=vendor")])[2].1, "-mod=vendor -p=2");
        assert_eq!(with(&[("GOFLAGS", "-p=8")]).len(), 2);
        // An empty value: only an environment GOFLAGS gets -p.
        let mut file_has_p = response();
        file_has_p.variables[2].value = String::new();
        assert_eq!(assemble(&file_has_p, |_| None).len(), 2);
        let env = |name: &str| (name == "GOFLAGS").then(|| OsString::from("-race"));
        assert_eq!(assemble(&file_has_p, env)[2].1, "-race -p=2");
        // Under a make's jobserver, cargo keeps it.
        let under = with(&[("MAKEFLAGS", "-j4 --jobserver-auth=3,4")]);
        assert!(!under.iter().any(|(name, _)| name == "CARGO_MAKEFLAGS"));
        // Disabled: nothing.
        let off = GetBuildEnvironmentResponse {
            enabled: false,
            ..response()
        };
        assert_eq!(assemble(&off, |_| None), Vec::<(String, String)>::new());
    }

    #[test]
    fn quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
