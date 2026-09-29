use std::fmt;
use std::str::FromStr;

use adam_error::{Classify, ErrorClass};

/// Which halves of the system one process runs.
///
/// The enum is closed on purpose: a new role must fail to compile in every host that matches
/// on it, so each host decides what the role runs. Hosts that only ask
/// [`runs_control_plane`](Self::runs_control_plane) and [`runs_workers`](Self::runs_workers)
/// need no change.
///
/// The default is [`Role::All`]: one process runs everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case")
)]
pub enum Role {
    /// The control plane and the workers, in one process.
    #[default]
    All,
    /// Only the control plane: the network front and the state changes users ask for.
    ControlPlane,
    /// Only the workers: the loops that claim and run work.
    Worker,
}

impl Role {
    /// Every role, in the order of the enum.
    pub const VALUES: [Role; 3] = [Role::All, Role::ControlPlane, Role::Worker];

    /// The stable name: `all`, `control-plane` or `worker`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::All => "all",
            Role::ControlPlane => "control-plane",
            Role::Worker => "worker",
        }
    }

    /// Whether this process runs the control plane.
    pub const fn runs_control_plane(self) -> bool {
        matches!(self, Role::All | Role::ControlPlane)
    }

    /// Whether this process runs the workers.
    pub const fn runs_workers(self) -> bool {
        matches!(self, Role::All | Role::Worker)
    }

    /// Parse a value the host read from its own place (an env var, a config file).
    ///
    /// `None` and a blank string give [`Role::All`]. Anything else must be a role name.
    pub fn from_optional(raw: Option<&str>) -> Result<Role, ParseRoleError> {
        match raw {
            Some(raw) if !raw.trim().is_empty() => raw.parse(),
            _ => Ok(Role::All),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = ParseRoleError;

    /// Trims the input and ignores ASCII case. Only the exact names are accepted, no aliases.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim();
        Role::VALUES
            .into_iter()
            .find(|role| role.as_str().eq_ignore_ascii_case(name))
            .ok_or_else(|| ParseRoleError {
                input: s.to_owned(),
            })
    }
}

fn accepted() -> String {
    let names: Vec<&str> = Role::VALUES.iter().map(|role| role.as_str()).collect();
    names.join(", ")
}

/// The text is not a role name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown role {input:?}; accepted values: {}", accepted())]
pub struct ParseRoleError {
    /// The text as given.
    pub input: String,
}

impl Classify for ParseRoleError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_round_trips_through_display_and_from_str() {
        for role in Role::VALUES {
            assert_eq!(role.to_string(), role.as_str());
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
        }
        assert_eq!(Role::VALUES.len(), 3);
    }

    #[test]
    fn the_names_are_stable() {
        assert_eq!(Role::All.as_str(), "all");
        assert_eq!(Role::ControlPlane.as_str(), "control-plane");
        assert_eq!(Role::Worker.as_str(), "worker");
    }

    #[test]
    fn the_default_is_all() {
        assert_eq!(Role::default(), Role::All);
    }

    #[test]
    fn parsing_trims_and_ignores_ascii_case() {
        assert_eq!(
            "  Control-Plane\n".parse::<Role>().unwrap(),
            Role::ControlPlane
        );
        assert_eq!("WORKER".parse::<Role>().unwrap(), Role::Worker);
        assert_eq!("\tAll ".parse::<Role>().unwrap(), Role::All);
    }

    #[test]
    fn only_exact_names_parse() {
        for bad in [
            "",
            " ",
            "x",
            "controlplane",
            "control_plane",
            "cp",
            "workers",
            "control plane",
        ] {
            assert!(bad.parse::<Role>().is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn from_optional_defaults_to_all_for_none_and_blank() {
        assert_eq!(Role::from_optional(None).unwrap(), Role::All);
        assert_eq!(Role::from_optional(Some("")).unwrap(), Role::All);
        assert_eq!(Role::from_optional(Some("  \t")).unwrap(), Role::All);
        assert_eq!(Role::from_optional(Some("worker")).unwrap(), Role::Worker);
        assert_eq!(
            Role::from_optional(Some(" Control-Plane ")).unwrap(),
            Role::ControlPlane
        );
    }

    #[test]
    fn an_unknown_role_is_an_error_that_lists_the_values() {
        let err = Role::from_optional(Some("x")).unwrap_err();
        assert_eq!(err.input, "x");
        let text = err.to_string();
        assert!(text.contains("\"x\""), "{text}");
        for role in Role::VALUES {
            assert!(text.contains(role.as_str()), "{text}");
        }
    }

    #[test]
    fn the_error_keeps_the_input_as_given() {
        let err = " Boss ".parse::<Role>().unwrap_err();
        assert_eq!(err.input, " Boss ");
    }

    #[test]
    fn which_halves_each_role_runs() {
        // (role, control plane, workers). Exhaustive: a new role forces a decision here.
        let table = |role: Role| match role {
            Role::All => (true, true),
            Role::ControlPlane => (true, false),
            Role::Worker => (false, true),
        };
        for role in Role::VALUES {
            let (cp, workers) = table(role);
            assert_eq!(role.runs_control_plane(), cp, "{role}");
            assert_eq!(role.runs_workers(), workers, "{role}");
            assert!(cp || workers, "{role} must run something");
        }
    }

    #[test]
    fn parse_role_error_is_invalid() {
        let err = ParseRoleError { input: "x".into() };
        assert_eq!(err.class(), ErrorClass::Invalid);
        assert!(!err.is_retryable());
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_value_names_are_kebab_case() {
        use clap::ValueEnum;

        let names: Vec<String> = Role::value_variants()
            .iter()
            .map(|role| role.to_possible_value().unwrap().get_name().to_owned())
            .collect();
        assert_eq!(names, ["all", "control-plane", "worker"]);
        for role in Role::VALUES {
            assert_eq!(
                <Role as ValueEnum>::from_str(role.as_str(), false).unwrap(),
                role
            );
        }
        assert_eq!(
            <Role as ValueEnum>::from_str("Control-Plane", true).unwrap(),
            Role::ControlPlane
        );
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_parses_a_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[arg(long, value_enum, default_value_t)]
            role: Role,
        }
        assert_eq!(Cli::try_parse_from(["host"]).unwrap().role, Role::All);
        assert_eq!(
            Cli::try_parse_from(["host", "--role", "control-plane"])
                .unwrap()
                .role,
            Role::ControlPlane
        );
        assert!(Cli::try_parse_from(["host", "--role", "boss"]).is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_uses_kebab_case() {
        for role in Role::VALUES {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(serde_json::from_str::<Role>(&json).unwrap(), role);
        }
        assert!(serde_json::from_str::<Role>("\"ControlPlane\"").is_err());
        assert!(serde_json::from_str::<Role>("\"boss\"").is_err());
    }
}
