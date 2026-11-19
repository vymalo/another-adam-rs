//! Which tool calls may start before the model has finished its turn (ADR 0049, amended
//! 2026-11-18). Pure: no I/O, no clock.
//!
//! A client says [`ModelDelta::ToolCallComplete`](adam_model::ModelDelta::ToolCallComplete) once per
//! call, when its arguments are whole, so the loop can start the call while the model is still
//! writing. [`Gate`] is the one place that decides: a call starts early only when starting it early
//! cannot change what the turn would have done. Every other call waits for the turn, as before.
//!
//! The conditions, all of which must hold:
//!
//! * the limit is above 1, and fewer calls than the limit are running
//!   ([`LlmAgentBuilder::max_parallel`](crate::LlmAgentBuilder::max_parallel)), and the call does
//!   not conflict with one that started early and is still running, nor with an earlier call of the
//!   turn that did not start early (ADR 0049 decision 1: two conflicting calls never overlap, and
//!   decision 5: a call waits for every earlier call it conflicts with, so an early call must not
//!   run before one that will run at the turn);
//! * the turn is not the closing one, and the run is not cancelled;
//! * the call takes a step at all (not a shortcut: arguments that are not JSON, a tool nobody has);
//! * the call is not a barrier: its [`Tool::access`](crate::Tool::access) is not
//!   [`ToolAccess::Alone`](crate::ToolAccess::Alone), it does not say `after_previous: true`
//!   ([`order`](crate::order)), and its key differs from the call before it (the committed
//!   `repeats.key` for the first call of the turn, which is what `doom_loop` needs the result of);
//! * no earlier call of the turn was a barrier: the call is in the first segment of the turn
//!   ([`schedule::segments`](crate::schedule::segments));
//! * the run's `max_tool_calls` still has room for it at its position in the turn;
//! * for the first call of the turn only, `doom_loop` would not refuse it: for a later call that
//!   decision needs the result of the one before it, which does not exist yet.
//!
//! A call whose `after_previous` is not a boolean does not run at all
//! ([`Order::Bad`](crate::order::Order)), so it is not started early either. The position of a call
//! is the order the client announced it, which is call order unless a provider interleaves its
//! indices; the turn then checks the batch against the calls it recorded (see
//! [`Early`](crate::agent::Early)).

use crate::access::ToolAccess;
use crate::order::Order;

/// What the turn allows, read once when it begins.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Turn {
    /// [`LlmAgentBuilder::max_parallel`](crate::LlmAgentBuilder::max_parallel).
    pub(crate) limit: usize,
    /// The closing turn may call nothing.
    pub(crate) closing: bool,
    /// The run was cancelled.
    pub(crate) cancelled: bool,
    /// How many tool calls the run has made ([`Conversation::tool_calls`](crate::Conversation)).
    pub(crate) tool_calls: u32,
    /// [`Limits::max_tool_calls`](crate::Limits::max_tool_calls).
    pub(crate) max_tool_calls: u32,
}

/// What one call of the turn says about itself, as it streams in.
pub(crate) struct Candidate<'a> {
    /// What the call said about its order: [`order::take`] on a clone of its arguments, unless the
    /// tool has an `after_previous` of its own ([`Order::Plain`] then).
    pub(crate) after_previous: Order,
    /// The call takes no step: its arguments were not JSON, or nobody has the tool.
    pub(crate) shortcut: bool,
    /// What the call shares (`LlmAgent::access_of`).
    pub(crate) access: &'a ToolAccess,
    /// The call's key (`call_key`), with `after_previous` taken out.
    pub(crate) key: &'a str,
    /// `doom_loop` would refuse this call if it were the first of the turn, from the committed
    /// state alone.
    pub(crate) doom_loop: bool,
    /// How many early calls are running now.
    pub(crate) running: usize,
    /// The call conflicts with one that started early and is still running, or with an earlier
    /// call of the turn that did not start early (it runs at the turn, after this one).
    pub(crate) conflicts: bool,
}

/// The gate of one turn: what it has seen so far.
#[derive(Debug)]
pub(crate) struct Gate {
    turn: Turn,
    /// The key of the call before (the committed `repeats.key` for the first call of the turn).
    previous: Option<String>,
    /// An earlier call of the turn was a barrier: nothing after it starts early.
    barred: bool,
}

impl Gate {
    /// A gate for `turn`, whose first call follows the committed call of `committed` (the
    /// `repeats.key` the state holds, when it holds one).
    pub(crate) fn new(turn: Turn, committed: Option<String>) -> Self {
        Self {
            turn,
            previous: committed,
            barred: false,
        }
    }

    /// The run was cancelled: no call starts from now on (ADR 0049 decision 10).
    pub(crate) fn cancel(&mut self) {
        self.turn.cancelled = true;
    }

    /// Whether the call at `index` (0-based, in the order the client announced it) may start now.
    /// Notes it either way: a barrier refuses every call after it, and every call becomes the one
    /// the next is compared with.
    pub(crate) fn admits(&mut self, index: usize, candidate: &Candidate<'_>) -> bool {
        let barrier = candidate.access.is_alone()
            || matches!(candidate.after_previous, Order::After)
            || self.previous.as_deref() == Some(candidate.key);
        let room = u32::try_from(index.saturating_add(1)).is_ok_and(|position| {
            self.turn.tool_calls.saturating_add(position) <= self.turn.max_tool_calls
        });
        let early = !self.barred
            && !barrier
            && !candidate.shortcut
            && !matches!(candidate.after_previous, Order::Bad(_))
            && self.turn.limit > 1
            && !self.turn.closing
            && !self.turn.cancelled
            && candidate.running < self.turn.limit
            && !candidate.conflicts
            && room
            && !(index == 0 && candidate.doom_loop);
        self.barred |= barrier;
        self.previous = Some(candidate.key.to_owned());
        early
    }
}

#[cfg(test)]
mod tests {
    use adam_model::ToolCall;
    use serde_json::{Value, json};

    use super::*;
    use crate::order;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::new("c", name, arguments)
    }

    fn read(scope: &str) -> ToolAccess {
        ToolAccess::reads(scope)
    }

    fn turn() -> Turn {
        Turn {
            limit: 10,
            closing: false,
            cancelled: false,
            tool_calls: 0,
            max_tool_calls: 200,
        }
    }

    /// One row of the table: a call, the turn it is in, and what the gate must say.
    struct Case {
        name: &'static str,
        turn: Turn,
        index: usize,
        call: ToolCall,
        shortcut: bool,
        access: ToolAccess,
        /// The tool has an `after_previous` of its own: the loop does not read one.
        own_order: bool,
        doom_loop: bool,
        running: usize,
        conflicts: bool,
        committed: Option<String>,
        expected: bool,
    }

    impl Case {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                turn: turn(),
                index: 0,
                call: call("read", json!({"path": "a"})),
                shortcut: false,
                access: ToolAccess::Independent,
                own_order: false,
                doom_loop: false,
                running: 0,
                conflicts: false,
                committed: None,
                expected: true,
            }
        }

        /// The key the loop gives the call: `after_previous` is taken out.
        fn key(&self) -> String {
            let mut call = self.call.clone();
            order::take(&mut call.arguments);
            crate::agent::call_key(&call)
        }

        fn admitted(&self) -> bool {
            let key = self.key();
            let after_previous = if self.own_order {
                Order::Plain
            } else {
                let mut arguments = self.call.arguments.clone();
                order::take(&mut arguments)
            };
            let candidate = Candidate {
                after_previous,
                shortcut: self.shortcut,
                access: &self.access,
                key: &key,
                doom_loop: self.doom_loop,
                running: self.running,
                conflicts: self.conflicts,
            };
            Gate::new(self.turn, self.committed.clone()).admits(self.index, &candidate)
        }
    }

    #[test]
    fn the_table_of_conditions() {
        let plain_key = Case::new("plain").key();
        let cases = [
            Case::new("an independent call starts early"),
            Case {
                turn: Turn { limit: 1, ..turn() },
                expected: false,
                ..Case::new("a limit of one does not")
            },
            Case {
                turn: Turn {
                    closing: true,
                    ..turn()
                },
                expected: false,
                ..Case::new("the closing turn starts nothing")
            },
            Case {
                turn: Turn {
                    cancelled: true,
                    ..turn()
                },
                expected: false,
                ..Case::new("a cancelled run starts nothing")
            },
            Case {
                shortcut: true,
                expected: false,
                ..Case::new("a call the turn answers without a step")
            },
            Case {
                access: ToolAccess::Alone,
                expected: false,
                ..Case::new("a tool that declares nothing")
            },
            Case {
                call: call("read", json!({"path": "a", "after_previous": true})),
                expected: false,
                ..Case::new("a call that asks to follow the ones before it")
            },
            Case {
                call: call("read", json!({"path": "a", "after_previous": "yes"})),
                expected: false,
                ..Case::new("a call whose after_previous is not a boolean does not run")
            },
            Case {
                call: call("t", json!({"after_previous": true})),
                own_order: true,
                expected: true,
                ..Case::new("a tool with its own after_previous keeps it and is no barrier")
            },
            Case {
                committed: Some(plain_key.clone()),
                expected: false,
                ..Case::new("a call that repeats the one before it")
            },
            Case {
                index: 1,
                doom_loop: true,
                expected: true,
                ..Case::new("doom_loop is only consulted for the first call")
            },
            Case {
                doom_loop: true,
                expected: false,
                ..Case::new("a doom-loop refusal is not started early")
            },
            Case {
                running: 10,
                expected: false,
                ..Case::new("the limit bounds the calls that run at once")
            },
            Case {
                conflicts: true,
                expected: false,
                ..Case::new("a call that conflicts with one already running")
            },
            Case {
                turn: Turn {
                    tool_calls: 200,
                    ..turn()
                },
                expected: false,
                ..Case::new("no room left under max_tool_calls")
            },
            Case {
                turn: Turn {
                    max_tool_calls: 1,
                    ..turn()
                },
                expected: true,
                ..Case::new("the last call that fits starts early")
            },
            Case {
                turn: Turn {
                    max_tool_calls: 1,
                    ..turn()
                },
                index: 1,
                expected: false,
                ..Case::new("the call over max_tool_calls does not")
            },
            Case {
                access: read("files/a"),
                ..Case::new("a call that declares a read shares nothing and starts early")
            },
        ];
        for case in cases {
            assert_eq!(case.admitted(), case.expected, "{}", case.name);
        }
    }

    #[test]
    fn a_barrier_refuses_every_call_after_it() {
        let first = call("write", json!({"path": "a"}));
        let second = call("read", json!({"path": "b"}));
        let mut gate = Gate::new(turn(), None);
        let key = crate::agent::call_key(&first);
        let alone = ToolAccess::Alone;
        let independent = ToolAccess::Independent;
        assert!(
            !gate.admits(
                0,
                &Candidate {
                    after_previous: Order::Plain,
                    shortcut: false,
                    access: &alone,
                    key: &key,
                    doom_loop: false,
                    running: 0,
                    conflicts: false,
                }
            ),
            "an alone call never starts early"
        );
        let key = crate::agent::call_key(&second);
        assert!(
            !gate.admits(
                1,
                &Candidate {
                    after_previous: Order::Plain,
                    shortcut: false,
                    access: &independent,
                    key: &key,
                    doom_loop: false,
                    running: 0,
                    conflicts: false,
                }
            ),
            "a call after a barrier waits for the turn"
        );
    }

    #[test]
    fn calls_of_one_segment_all_start_early() {
        let mut gate = Gate::new(turn(), None);
        for (index, name) in ["a", "b", "c"].iter().enumerate() {
            let call = call("read", json!({"path": name}));
            let key = crate::agent::call_key(&call);
            let independent = ToolAccess::Independent;
            assert!(gate.admits(
                index,
                &Candidate {
                    after_previous: Order::Plain,
                    shortcut: false,
                    access: &independent,
                    key: &key,
                    doom_loop: false,
                    running: index,
                    conflicts: false,
                }
            ));
        }
    }
}
