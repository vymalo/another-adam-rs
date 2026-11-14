//! The plan of a conversation: a short checklist the agent keeps for the person (ADR 0044 decision 5).
//!
//! The plan is **state**, not text in a prompt. A tool reads it with [`ToolCtx::plan`](crate::ToolCtx::plan)
//! and returns the new one with [`ToolOutput::with_plan`](crate::ToolOutput::with_plan); the loop
//! applies it from a result that is not an error, in the same commit as the result, so a replay
//! takes the journaled output and a run that continues another keeps it. The loop never writes it
//! into the system prompt and never rewrites an earlier message with it: the model reads the plan
//! in the result of each plan call, the newest at the end of the history (the stable prefix, ADR
//! 0044 decision 1).

use serde::{Deserialize, Serialize};

/// The most items a plan holds.
pub const MAX_PLAN_ITEMS: usize = 50;

/// Where an item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    Doing,
    /// Finished.
    Done,
    /// No longer needed.
    Dropped,
}

/// One line of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// The item's creation number: 1 for the first item a plan was written with, then one more for
    /// each item added. Never reused and never shifted, wherever the item is shown.
    pub id: u32,
    /// What to do, in one line.
    pub text: String,
    /// Where it stands.
    pub status: PlanStatus,
    /// A word of explanation (why an item was dropped, where the work is), if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The plan of a conversation: items in display order, under an optional title.
///
/// Nothing is ever deleted from a plan (a dropped item stays), so the ids are always `1..=n` for
/// the `n` items it has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The title, if the plan has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The items, in the order they are shown.
    pub items: Vec<PlanItem>,
}

impl Plan {
    /// The item numbered `id`.
    pub fn item(&self, id: u32) -> Option<&PlanItem> {
        self.items.iter().find(|item| item.id == id)
    }

    /// How many items have `status`.
    pub fn count(&self, status: PlanStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    /// The plan as short ASCII lines, the same bytes for the same plan:
    ///
    /// ```text
    /// Plan: Fix the date parser
    /// 1. [x] Reproduce the failing test
    /// 2. [>] Fix parse_date for time zones (note: lexer.rs)
    /// 3. [ ] Run the checks
    /// 4. [-] Open the pull request
    /// 1 of 4 done, 1 doing, 1 to do, 1 dropped.
    /// ```
    ///
    /// `[x]` is done, `[>]` doing, `[ ]` to do and `[-]` dropped; the first line is just `Plan` when the plan
    /// has no title. Items are in display order and carry their own numbers.
    pub fn render(&self) -> String {
        let mut out = match self.title.as_deref() {
            Some(title) => format!("Plan: {title}"),
            None => "Plan".to_owned(),
        };
        for item in &self.items {
            let mark = match item.status {
                PlanStatus::Pending => " ",
                PlanStatus::Doing => ">",
                PlanStatus::Done => "x",
                PlanStatus::Dropped => "-",
            };
            out.push_str(&format!("\n{}. [{mark}] {}", item.id, item.text));
            if let Some(note) = &item.note {
                out.push_str(&format!(" (note: {note})"));
            }
        }
        out.push_str(&format!(
            "\n{} of {} done, {} doing, {} to do, {} dropped.",
            self.count(PlanStatus::Done),
            self.items.len(),
            self.count(PlanStatus::Doing),
            self.count(PlanStatus::Pending),
            self.count(PlanStatus::Dropped),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_renders_the_same_bytes_for_the_same_state() {
        let plan = Plan {
            title: Some("Fix the date parser".into()),
            items: vec![
                item(1, "Reproduce the failing test", PlanStatus::Done, None),
                item(
                    2,
                    "Fix parse_date for time zones",
                    PlanStatus::Doing,
                    Some("lexer.rs"),
                ),
                item(5, "Add a test for UTC+14", PlanStatus::Pending, None),
                item(3, "Run the checks", PlanStatus::Pending, None),
                item(
                    4,
                    "Open the pull request",
                    PlanStatus::Dropped,
                    Some("the person opens it"),
                ),
            ],
        };
        assert_eq!(
            plan.render(),
            "Plan: Fix the date parser\n\
             1. [x] Reproduce the failing test\n\
             2. [>] Fix parse_date for time zones (note: lexer.rs)\n\
             5. [ ] Add a test for UTC+14\n\
             3. [ ] Run the checks\n\
             4. [-] Open the pull request (note: the person opens it)\n\
             1 of 5 done, 1 doing, 2 to do, 1 dropped."
        );
        assert_eq!(plan.render(), plan.clone().render());
    }

    #[test]
    fn an_untitled_plan_says_plan() {
        let plan = Plan {
            title: None,
            items: vec![item(1, "One", PlanStatus::Pending, None)],
        };
        assert_eq!(
            plan.render(),
            "Plan\n1. [ ] One\n0 of 1 done, 0 doing, 1 to do, 0 dropped."
        );
    }

    fn item(id: u32, text: &str, status: PlanStatus, note: Option<&str>) -> PlanItem {
        PlanItem {
            id,
            text: text.into(),
            status,
            note: note.map(str::to_owned),
        }
    }
}
