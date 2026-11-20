//! Schedules: `schedules/<name>.md`, one of `cron`, `at` or `after` in the frontmatter, the prompt
//! in the body (ADR 0060).

use std::path::Path;

use super::read_front;
use crate::diagnostic::Sink;
use crate::frontmatter::{clean_body, key_line};
use crate::manifest::Schedule;
use crate::schema::ScheduleFrontmatter;
use crate::when::{At, Cron, When, parse_after, parse_at};

/// Read one schedule. `name` is the path-derived name (`a/b`), `owner` the agent it belongs to.
pub(crate) fn parse_schedule(
    sink: &mut Sink<'_>,
    path: &Path,
    text: &str,
    name: &str,
    owner: &str,
) -> Option<Schedule> {
    let front = read_front::<ScheduleFrontmatter>(sink, text)?;
    let (fm, split) = (front.value, front.split);
    for key in fm.extra.keys() {
        sink.warn(
            key_line(&split, key),
            format!("unknown key `{key}` is ignored"),
        );
    }

    let mut ok = true;
    let (cron, at, after) = (given(&fm.cron), given(&fm.at), given(&fm.after));
    let named: Vec<&str> = [("cron", cron), ("at", at), ("after", after)]
        .into_iter()
        .filter_map(|(key, v)| v.map(|_| key))
        .collect();
    let when = match named.as_slice() {
        [] => {
            sink.error(Some(1), "a schedule needs one of `cron`, `at` or `after`");
            None
        }
        [_] => {
            let (key, value) = [("cron", cron), ("at", at), ("after", after)]
                .into_iter()
                .find_map(|(key, v)| v.map(|v| (key, v)))
                .unwrap_or_default();
            let parsed = match key {
                "cron" => Cron::parse(value).map(When::Cron),
                "at" => parse_at(value).map(When::At),
                _ => parse_after(value).map(When::After),
            };
            match parsed {
                Ok(when) => Some(when),
                Err(why) => {
                    sink.error(key_line(&split, key), format!("`{key}: {value}` {why}"));
                    None
                }
            }
        }
        [_, second, ..] => {
            sink.error(
                key_line(&split, second),
                format!(
                    "a schedule takes one of `cron`, `at` or `after`, and this one has `{}`",
                    named.join("` and `")
                ),
            );
            None
        }
    };
    ok &= when.is_some();
    let timezone = fm.timezone.as_deref().map_or("UTC", str::trim);
    if !is_zone_like(timezone) {
        sink.error(
            key_line(&split, "timezone"),
            format!("`timezone: {timezone}` is not an IANA time zone name such as `Europe/Berlin`"),
        );
        ok = false;
    } else if fm.timezone.is_some() {
        let ignored = match when {
            Some(When::After(_)) => Some("`after` counts from when the schedule starts"),
            Some(When::At(At::Instant(_))) => Some("`at` names its offset"),
            _ => None,
        };
        if let Some(why) = ignored {
            sink.warn(
                key_line(&split, "timezone"),
                format!("`timezone: {timezone}` is ignored: {why}"),
            );
        }
    }
    if let Some(agent) = fm.agent.as_deref().filter(|a| *a != owner) {
        sink.error(
            key_line(&split, "agent"),
            format!("`agent: {agent}` does not match the agent `{owner}` whose directory holds this schedule"),
        );
        ok = false;
    }
    let prompt = clean_body(split.body);
    if prompt.is_empty() {
        sink.error(
            Some(split.body_line),
            "a schedule needs a body: it is the prompt of each run",
        );
        ok = false;
    }
    ok.then(|| Schedule {
        name: name.to_owned(),
        cron: cron.unwrap_or_default().to_owned(),
        at: at.map(str::to_owned),
        after: after.map(str::to_owned),
        timezone: timezone.to_owned(),
        agent: owner.to_owned(),
        prompt,
        path: path.to_path_buf(),
    })
}

/// A key's value, trimmed; `None` when absent or blank.
fn given(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

fn is_zone_like(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() <= 64
        && zone
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones() {
        assert!(is_zone_like("UTC") && is_zone_like("Europe/Berlin") && is_zone_like("Etc/GMT+1"));
        assert!(!is_zone_like("") && !is_zone_like("Mars Base"));
    }
}
