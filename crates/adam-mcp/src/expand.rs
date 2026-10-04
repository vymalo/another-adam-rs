//! `${VAR}` and `${VAR:-default}` in the values of an `mcp.json`, expanded once, when the servers
//! are connected. The grammar is [`adam_agent_fs::split_env_references`]: the same spans the build
//! recorded as the names the file needs.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use adam_agent_fs::{EnvRef, Segment, split_env_references};
use secrecy::{ExposeSecret, SecretString};

use crate::error::{Error, VarProblem};
use crate::redact::Redactor;

/// Values for environment variables, taken before the process environment. A composition root
/// that fetches its secrets from a vault gives them here and never puts them in the environment.
/// The values are held as secrets: `Debug` shows the names only.
///
/// ```
/// use adam_mcp::Env;
///
/// let env = Env::new().var("LINEAR_API_TOKEN", "lin_api_example");
/// assert_eq!(format!("{env:?}"), r#"Env { names: ["LINEAR_API_TOKEN"] }"#);
/// ```
#[derive(Clone, Default)]
pub struct Env {
    values: Arc<BTreeMap<String, SecretString>>,
}

impl Env {
    /// No values: only the process environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Give `name` the value `value`.
    #[must_use]
    pub fn var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.values).insert(name.into(), SecretString::from(value.into()));
        self
    }

    /// The values `adam-assembly` keeps for [`AgentDef::env`](https://docs.rs/adam-assembly), so
    /// one set of secrets serves the remote subagents and the MCP servers of an agent.
    pub fn from_values(values: Arc<BTreeMap<String, SecretString>>) -> Self {
        Self { values }
    }
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Expands the texts of one server, and remembers what it substituted.
pub(crate) struct Expander<'a> {
    server: &'a str,
    env: &'a Env,
    redactor: &'a mut Redactor,
}

impl<'a> Expander<'a> {
    pub(crate) fn new(server: &'a str, env: &'a Env, redactor: &'a mut Redactor) -> Self {
        Self {
            server,
            env,
            redactor,
        }
    }

    /// What has been registered so far (to scrub what an error is about to show).
    pub(crate) fn redactor(&self) -> &Redactor {
        self.redactor
    }

    /// `text` with each reference replaced. A variable's value is registered with the redactor,
    /// and so is the whole expanded text when it contained one (a header `Bearer <token>`, a URL
    /// with a key in its query), so no third-party message can repeat either.
    pub(crate) fn expand(&mut self, text: &str) -> Result<String, Error> {
        self.expand_with(text, false)
    }

    /// [`expand`](Self::expand) for the value of a header: a `${VAR}` with no default whose
    /// variable is empty is an error ([`VarProblem::Empty`]), because the header would be sent
    /// with its credential missing (`Authorization: Bearer `) and the server's answer would be
    /// the only sign of it.
    pub(crate) fn expand_header(&mut self, text: &str) -> Result<String, Error> {
        self.expand_with(text, true)
    }

    fn expand_with(&mut self, text: &str, forbid_empty: bool) -> Result<String, Error> {
        let mut out = String::with_capacity(text.len());
        let mut from_variable = false;
        for segment in split_env_references(text) {
            match segment {
                Segment::Literal(literal) => out.push_str(literal),
                Segment::Ref(reference) => match self.value(&reference, forbid_empty)? {
                    Value::Variable(value) => {
                        self.redactor.add(&value);
                        out.push_str(&value);
                        from_variable = true;
                    }
                    // Text from the file itself: not a secret, and nothing to hide.
                    Value::Default(text) => out.push_str(&text),
                },
            }
        }
        if from_variable {
            self.redactor.add(&out);
        }
        Ok(out)
    }

    fn value(&self, reference: &EnvRef, forbid_empty: bool) -> Result<Value, Error> {
        let fail = |problem| Error::Var {
            server: self.server.to_owned(),
            var: reference.name.clone(),
            problem,
        };
        let set = match self.env.values.get(&reference.name) {
            Some(value) => Some(value.expose_secret().to_owned()),
            None => match std::env::var(&reference.name) {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err(fail(VarProblem::NotUnicode));
                }
            },
        };
        // POSIX `:-`: the default stands for a variable that is unset *or empty*. Without a
        // default, an empty variable expands to nothing, as in a shell, except in a header.
        match (set, &reference.default) {
            (Some(value), _) if !value.is_empty() => Ok(Value::Variable(value)),
            (_, Some(default)) => Ok(Value::Default(default.clone())),
            (Some(_), None) if forbid_empty => Err(fail(VarProblem::Empty)),
            (Some(_), None) => Ok(Value::Variable(String::new())),
            (None, None) => Err(fail(VarProblem::Missing)),
        }
    }
}

enum Value {
    /// What a variable held.
    Variable(String),
    /// The `:-default` written in the file.
    Default(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name no environment has.
    const UNSET: &str = "ADAM_MCP_TEST_SURELY_UNSET_7F3A";

    fn expand(env: &Env, text: &str) -> Result<(String, Redactor), Error> {
        let mut redactor = Redactor::default();
        let out = Expander::new("linear", env, &mut redactor).expand(text)?;
        Ok((out, redactor))
    }

    #[test]
    fn expands_from_env_before_process_env() {
        // `PATH` is in every process environment; the value given in code wins over it.
        let (from_code, _) = expand(&Env::new().var("PATH", "from-code"), "p=${PATH}!").unwrap();
        assert_eq!(from_code, "p=from-code!");
        let process = std::env::var("PATH").unwrap();
        let (from_process, _) = expand(&Env::new(), "p=${PATH}!").unwrap();
        assert_eq!(from_process, format!("p={process}!"));
        // Text without references passes through, and so does a lone `$`.
        assert_eq!(expand(&Env::new(), "cost $5 {x}").unwrap().0, "cost $5 {x}");
    }

    #[test]
    fn default_applies_when_unset_or_empty() {
        let env = Env::new().var("EMPTY", "").var("SET", "value");
        assert_eq!(
            expand(&env, &format!("${{{UNSET}:-fallback}}")).unwrap().0,
            "fallback"
        );
        assert_eq!(expand(&env, "${EMPTY:-fallback}").unwrap().0, "fallback");
        assert_eq!(expand(&env, "${SET:-fallback}").unwrap().0, "value");
        // An empty default is a default: unset gives nothing, and no error.
        assert_eq!(expand(&env, &format!("[${{{UNSET}:-}}]")).unwrap().0, "[]");
        // Without a default, an empty variable is empty (as in a shell), not missing.
        assert_eq!(expand(&env, "[${EMPTY}]").unwrap().0, "[]");
    }

    #[test]
    fn an_empty_variable_is_an_error_in_a_header_unless_it_has_a_default() {
        let env = Env::new().var("EMPTY", "").var("SET", "tok");
        let header = |text: &str| {
            let mut redactor = Redactor::default();
            Expander::new("search", &env, &mut redactor).expand_header(text)
        };
        let error = header("Bearer ${EMPTY}").unwrap_err();
        assert!(matches!(
            &error,
            Error::Var { server, var, problem: VarProblem::Empty } if server == "search" && var == "EMPTY"
        ));
        assert!(error.to_string().contains("is empty"), "{error}");
        assert_eq!(header("Bearer ${SET}").unwrap(), "Bearer tok");
        assert_eq!(header("Bearer ${EMPTY:-dev}").unwrap(), "Bearer dev");
        // Unset stays the error it was, and elsewhere an empty variable is still empty.
        assert!(matches!(
            header(&format!("${{{UNSET}}}")).unwrap_err(),
            Error::Var {
                problem: VarProblem::Missing,
                ..
            }
        ));
        assert_eq!(expand(&env, "[${EMPTY}]").unwrap().0, "[]");
    }

    #[test]
    fn missing_var_names_the_variable_only() {
        let env = Env::new().var("OTHER", "sekrit-value");
        let error = expand(&env, &format!("Bearer ${{{UNSET}}} and ${{OTHER}}")).unwrap_err();
        assert!(matches!(
            &error,
            Error::Var { server, var, problem: VarProblem::Missing } if server == "linear" && var == UNSET
        ));
        let text = error.to_string();
        assert!(text.contains(UNSET) && text.contains("linear"), "{text}");
        assert!(!text.contains("sekrit-value"), "{text}");
    }

    #[test]
    fn malformed_references_stay_literal() {
        let env = Env::new().var("A", "1");
        let text = "${1X} ${ } ${A} ${open";
        assert_eq!(expand(&env, text).unwrap().0, "${1X} ${ } 1 ${open");
    }

    #[test]
    fn substituted_values_are_registered_but_defaults_are_not() {
        let env = Env::new().var("TOKEN", "abc-123-secret");
        let (out, redactor) = expand(&env, "Bearer ${TOKEN}").unwrap();
        assert_eq!(out, "Bearer abc-123-secret");
        assert_eq!(
            redactor.scrub("bad header `Bearer abc-123-secret`, key abc-123-secret"),
            "bad header `[REDACTED]`, key [REDACTED]"
        );
        let (_, redactor) = expand(&Env::new(), &format!("${{{UNSET}:-info}}")).unwrap();
        assert_eq!(redactor.scrub("info"), "info");
    }

    #[test]
    fn debug_shows_names_only() {
        let env = Env::new().var("A_TOKEN", "sekrit-value").var("B", "x");
        let shown = format!("{env:?}");
        assert!(
            shown.contains("A_TOKEN") && !shown.contains("sekrit-value"),
            "{shown}"
        );
    }
}
