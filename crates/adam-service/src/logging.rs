//! The log filter every agent binary starts with.

use tracing_subscriber::EnvFilter;

/// The filter used when `RUST_LOG` is not set: `info`, and `warn` for `rmcp`.
///
/// `rmcp` (the MCP client) logs at INFO a whole `peer_info` ("Service initialized as client"), then
/// "task cancelled" and "serve finished", **every time** a connection opens and closes, and the
/// thread-tools endpoint is connected at every model turn: one turn is three lines of noise, and the
/// peer's description is not for a log. Its warnings and errors still show.
pub const DEFAULT_LOG_FILTER: &str = "info,rmcp=warn";

/// The filter of the process: `RUST_LOG` when it is set and valid (it replaces the default whole,
/// so `RUST_LOG=info` brings `rmcp` back at `info`), else [`DEFAULT_LOG_FILTER`].
#[must_use]
pub fn env_filter() -> EnvFilter {
    filter_from(EnvFilter::try_from_default_env().ok())
}

/// `from_env` when there is one, else [`DEFAULT_LOG_FILTER`].
fn filter_from(from_env: Option<EnvFilter>) -> EnvFilter {
    from_env.unwrap_or_else(|| EnvFilter::new(DEFAULT_LOG_FILTER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;
    use tracing_subscriber::filter::Targets;

    #[test]
    fn rmcp_is_quiet_by_default_and_everything_else_is_info() {
        // The same directives, read as targets: what each level of each crate does.
        let targets: Targets = DEFAULT_LOG_FILTER.parse().unwrap();
        assert!(!targets.would_enable("rmcp::service", &Level::INFO));
        assert!(!targets.would_enable("rmcp", &Level::INFO));
        assert!(targets.would_enable("rmcp::service", &Level::WARN));
        assert!(targets.would_enable("rmcp::service", &Level::ERROR));
        assert!(targets.would_enable("adam_coder", &Level::INFO));
        assert!(targets.would_enable("adam_ui::thread_tools", &Level::INFO));
        assert!(!targets.would_enable("adam_coder", &Level::DEBUG));
        // And the filter the process builds has the same two directives (it prints them in its own
        // order).
        let built = filter_from(None).to_string();
        assert!(built.contains("rmcp=warn"), "{built}");
        assert!(built.split(',').any(|d| d == "info"), "{built}");
    }

    #[test]
    fn the_environment_replaces_the_default_whole() {
        let chosen = filter_from(Some(EnvFilter::new("debug")));
        assert_eq!(chosen.to_string(), "debug");
    }
}
