//! Schedules: `schedules/<name>.md`, cron in the frontmatter, the prompt in the body.

use std::path::Path;

use super::read_front;
use crate::diagnostic::Sink;
use crate::frontmatter::{clean_body, key_line};
use crate::manifest::Schedule;
use crate::schema::ScheduleFrontmatter;

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
    let cron = fm.cron.as_deref().map(str::trim).unwrap_or_default();
    if cron.is_empty() {
        sink.error(Some(1), "a schedule needs `cron`");
        ok = false;
    } else if let Err(why) = check_cron(cron) {
        sink.error(key_line(&split, "cron"), format!("`cron: {cron}` {why}"));
        ok = false;
    }
    let timezone = fm.timezone.as_deref().map_or("UTC", str::trim);
    if !is_zone_like(timezone) {
        sink.error(
            key_line(&split, "timezone"),
            format!("`timezone: {timezone}` is not an IANA time zone name such as `Europe/Berlin`"),
        );
        ok = false;
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
        cron: cron.to_owned(),
        timezone: timezone.to_owned(),
        agent: owner.to_owned(),
        prompt,
        path: path.to_path_buf(),
    })
}

/// Five whitespace-separated fields of cron characters. The expression is not evaluated here.
fn check_cron(cron: &str) -> Result<(), String> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "has {} fields; a schedule takes five (minute hour day-of-month month day-of-week)",
            fields.len()
        ));
    }
    for f in fields {
        if !f
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | ',' | '-' | '/' | '?'))
        {
            return Err(format!("has an invalid field `{f}`"));
        }
    }
    Ok(())
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
    fn cron_shapes() {
        assert!(check_cron("0 9 * * 1-5").is_ok());
        assert!(check_cron("*/5 * * * MON,TUE").is_ok());
        assert!(check_cron("0 9 * *").is_err());
        assert!(check_cron("0 9 * * * *").is_err());
        assert!(check_cron("0 9 * * $").is_err());
    }

    #[test]
    fn zones() {
        assert!(is_zone_like("UTC") && is_zone_like("Europe/Berlin") && is_zone_like("Etc/GMT+1"));
        assert!(!is_zone_like("") && !is_zone_like("Mars Base"));
    }
}
