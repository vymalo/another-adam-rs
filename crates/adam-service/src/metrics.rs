//! The process's pools as metrics ([ADR 0065] decision 9).
//!
//! A server exposes one series per pool for what it holds, its ceiling and what waits, and the
//! token bucket when the process meters tokens per minute. It names no person, no run and no
//! conversation: a conversation's share is enforced by the admission, never a label here.
//!
//! [`render`] writes the Prometheus text exposition format. `adam-service` runs no metrics
//! listener of its own: a composition that serves one renders this on request.
//!
//! Not built yet, and named so a reader knows what is missing: `adam_admission_waits_total`,
//! `adam_admission_refusals_total`, `adam_process_stopped_total` and `adam_disk_bytes` are
//! counters and sizes the admission does not keep. [`Usage`] carries what is held now, not a
//! running total, so this renders gauges only.
//!
//! [ADR 0065]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md

use std::fmt::Write as _;

use adam_budget::Usage;

/// Where a limit prints as no limit: Prometheus' own infinity, never a zero a reader would take
/// for a ceiling.
const UNLIMITED: &str = "+Inf";

/// The pools, their ceilings, what waits and the token bucket of `usage`, in the Prometheus text
/// exposition format.
#[must_use]
pub fn render(usage: &Usage) -> String {
    let mut out = String::new();
    out.push_str(
        "# HELP adam_admission_in_use The share of a pool held now, in the pool's unit.\n\
         # TYPE adam_admission_in_use gauge\n",
    );
    for line in usage.pools() {
        let _ = writeln!(
            out,
            "adam_admission_in_use{{pool=\"{}\"}} {}",
            line.pool().as_str(),
            line.in_use()
        );
    }
    out.push_str(
        "# HELP adam_admission_limit The ceiling of a pool, in its unit.\n\
         # TYPE adam_admission_limit gauge\n",
    );
    for line in usage.pools() {
        let limit = line
            .limit()
            .at_most()
            .map_or_else(|| UNLIMITED.to_owned(), |limit| limit.to_string());
        let _ = writeln!(
            out,
            "adam_admission_limit{{pool=\"{}\"}} {limit}",
            line.pool().as_str()
        );
    }
    out.push_str(
        "# HELP adam_admission_waiting The asks waiting for room in a pool now.\n\
         # TYPE adam_admission_waiting gauge\n",
    );
    for line in usage.pools() {
        let _ = writeln!(
            out,
            "adam_admission_waiting{{pool=\"{}\"}} {}",
            line.pool().as_str(),
            line.waiting()
        );
    }
    if let Some(tokens) = usage.tokens() {
        out.push_str(
            "# HELP adam_admission_tokens_available The tokens available in the per-minute bucket now.\n\
             # TYPE adam_admission_tokens_available gauge\n",
        );
        let _ = writeln!(
            out,
            "adam_admission_tokens_available {}",
            tokens.available()
        );
        out.push_str(
            "# HELP adam_admission_tokens_limit The bucket's size, in tokens per minute.\n\
             # TYPE adam_admission_tokens_limit gauge\n",
        );
        let _ = writeln!(out, "adam_admission_tokens_limit {}", tokens.limit());
    }
    out
}
