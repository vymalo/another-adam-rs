use std::fmt;
use std::str::FromStr;

use adam_error::{Classify, ErrorClass};

/// Where the files of a run live, and whether a run stays on one worker.
///
/// The deployer chooses; the host reads the choice from its own place (an env var, a config
/// file) and passes it to [`Placement::from_optional`]. The enum is closed on purpose: a new
/// placement must fail to compile in every host that matches on it. Hosts that only ask
/// [`pins_runs`](Self::pins_runs) and [`needs_workspace`](Self::needs_workspace) need no change.
///
/// The default is [`Placement::Shared`].
///
/// See [ADR 0002](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0002-workspace-placement.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case")
)]
pub enum Placement {
    /// One volume that every worker mounts; any worker may resume any run.
    #[default]
    Shared,
    /// A run is stepped only by the worker that first claimed it, and that worker owns a
    /// folder of its own under the workspace root.
    Affinity,
    /// As [`Placement::Affinity`], and the worker's whole root is a volume of its own.
    Isolated,
    /// No workspace: the host's agents only call remote agents.
    A2aOnly,
}

impl Placement {
    /// Every placement, in the order of the enum.
    pub const VALUES: [Placement; 4] = [
        Placement::Shared,
        Placement::Affinity,
        Placement::Isolated,
        Placement::A2aOnly,
    ];

    /// The stable name: `shared`, `affinity`, `isolated` or `a2a-only`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Placement::Shared => "shared",
            Placement::Affinity => "affinity",
            Placement::Isolated => "isolated",
            Placement::A2aOnly => "a2a-only",
        }
    }

    /// Whether a run must stay on the worker that first claimed it.
    ///
    /// A host maps this to `ClaimScope::Pinned` in `adam-core`. A pinning worker needs a
    /// stable worker id.
    pub const fn pins_runs(self) -> bool {
        matches!(self, Placement::Affinity | Placement::Isolated)
    }

    /// Whether the host has a workspace (a filesystem root) at all.
    pub const fn needs_workspace(self) -> bool {
        !matches!(self, Placement::A2aOnly)
    }

    /// Parse a value the host read from its own place (an env var, a config file).
    ///
    /// `None` and a blank string give [`Placement::Shared`]. Anything else must be a name.
    pub fn from_optional(raw: Option<&str>) -> Result<Placement, ParsePlacementError> {
        match raw {
            Some(raw) if !raw.trim().is_empty() => raw.parse(),
            _ => Ok(Placement::Shared),
        }
    }
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Placement {
    type Err = ParsePlacementError;

    /// Trims the input and ignores ASCII case. Only the exact names are accepted, no aliases.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim();
        Placement::VALUES
            .into_iter()
            .find(|placement| placement.as_str().eq_ignore_ascii_case(name))
            .ok_or_else(|| ParsePlacementError {
                input: s.to_owned(),
            })
    }
}

fn accepted() -> String {
    let names: Vec<&str> = Placement::VALUES.iter().map(|p| p.as_str()).collect();
    names.join(", ")
}

/// The text is not a placement name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown placement {input:?}; accepted values: {}", accepted())]
pub struct ParsePlacementError {
    /// The text as given.
    pub input: String,
}

impl Classify for ParsePlacementError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_round_trips_through_display_and_from_str() {
        for placement in Placement::VALUES {
            assert_eq!(placement.to_string(), placement.as_str());
            assert_eq!(
                placement.to_string().parse::<Placement>().unwrap(),
                placement
            );
        }
        assert_eq!(Placement::VALUES.len(), 4);
    }

    #[test]
    fn the_names_are_stable() {
        assert_eq!(Placement::Shared.as_str(), "shared");
        assert_eq!(Placement::Affinity.as_str(), "affinity");
        assert_eq!(Placement::Isolated.as_str(), "isolated");
        assert_eq!(Placement::A2aOnly.as_str(), "a2a-only");
    }

    #[test]
    fn the_default_is_shared() {
        assert_eq!(Placement::default(), Placement::Shared);
    }

    #[test]
    fn parsing_trims_and_ignores_ascii_case() {
        assert_eq!(
            "  A2A-Only\n".parse::<Placement>().unwrap(),
            Placement::A2aOnly
        );
        assert_eq!(
            "ISOLATED".parse::<Placement>().unwrap(),
            Placement::Isolated
        );
        assert_eq!("\tShared ".parse::<Placement>().unwrap(), Placement::Shared);
    }

    #[test]
    fn only_exact_names_parse() {
        for bad in [
            "",
            " ",
            "x",
            "a2a",
            "a2aonly",
            "a2a_only",
            "a2a only",
            "pinned",
            "isolate",
            "affinities",
        ] {
            assert!(bad.parse::<Placement>().is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn from_optional_defaults_to_shared_for_none_and_blank() {
        assert_eq!(Placement::from_optional(None).unwrap(), Placement::Shared);
        assert_eq!(
            Placement::from_optional(Some("")).unwrap(),
            Placement::Shared
        );
        assert_eq!(
            Placement::from_optional(Some("  \t")).unwrap(),
            Placement::Shared
        );
        assert_eq!(
            Placement::from_optional(Some("affinity")).unwrap(),
            Placement::Affinity
        );
        assert_eq!(
            Placement::from_optional(Some(" A2a-Only ")).unwrap(),
            Placement::A2aOnly
        );
    }

    #[test]
    fn an_unknown_placement_is_an_error_that_lists_the_values() {
        let err = Placement::from_optional(Some("x")).unwrap_err();
        assert_eq!(err.input, "x");
        let text = err.to_string();
        assert!(text.contains("\"x\""), "{text}");
        for placement in Placement::VALUES {
            assert!(text.contains(placement.as_str()), "{text}");
        }
    }

    #[test]
    fn the_error_keeps_the_input_as_given() {
        let err = " Boss ".parse::<Placement>().unwrap_err();
        assert_eq!(err.input, " Boss ");
    }

    #[test]
    fn what_each_placement_needs() {
        // (placement, pins runs, needs a workspace). Exhaustive: a new placement forces a
        // decision here.
        let table = |placement: Placement| match placement {
            Placement::Shared => (false, true),
            Placement::Affinity => (true, true),
            Placement::Isolated => (true, true),
            Placement::A2aOnly => (false, false),
        };
        for placement in Placement::VALUES {
            let (pins, workspace) = table(placement);
            assert_eq!(placement.pins_runs(), pins, "{placement}");
            assert_eq!(placement.needs_workspace(), workspace, "{placement}");
            assert!(
                !pins || workspace,
                "{placement} pins runs to a folder it lacks"
            );
        }
    }

    #[test]
    fn parse_placement_error_is_invalid() {
        let err = ParsePlacementError { input: "x".into() };
        assert_eq!(err.class(), ErrorClass::Invalid);
        assert!(!err.is_retryable());
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_value_names_are_kebab_case() {
        use clap::ValueEnum;

        let names: Vec<String> = Placement::value_variants()
            .iter()
            .map(|p| p.to_possible_value().unwrap().get_name().to_owned())
            .collect();
        assert_eq!(names, ["shared", "affinity", "isolated", "a2a-only"]);
        for placement in Placement::VALUES {
            assert_eq!(
                <Placement as ValueEnum>::from_str(placement.as_str(), false).unwrap(),
                placement
            );
        }
        assert_eq!(
            <Placement as ValueEnum>::from_str("A2a-Only", true).unwrap(),
            Placement::A2aOnly
        );
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_parses_a_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[arg(long, value_enum, default_value_t)]
            placement: Placement,
        }
        assert_eq!(
            Cli::try_parse_from(["host"]).unwrap().placement,
            Placement::Shared
        );
        assert_eq!(
            Cli::try_parse_from(["host", "--placement", "a2a-only"])
                .unwrap()
                .placement,
            Placement::A2aOnly
        );
        assert!(Cli::try_parse_from(["host", "--placement", "boss"]).is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_uses_kebab_case() {
        for placement in Placement::VALUES {
            let json = serde_json::to_string(&placement).unwrap();
            assert_eq!(json, format!("\"{}\"", placement.as_str()));
            assert_eq!(serde_json::from_str::<Placement>(&json).unwrap(), placement);
        }
        assert!(serde_json::from_str::<Placement>("\"A2aOnly\"").is_err());
        assert!(serde_json::from_str::<Placement>("\"boss\"").is_err());
    }
}
