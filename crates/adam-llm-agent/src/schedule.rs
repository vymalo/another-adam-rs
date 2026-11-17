//! The schedule of the tool calls of one model turn: pure, no I/O, no clock (ADR 0049).
//!
//! * [`segments`] cuts the turn at its **barriers**: a call that runs alone (it says
//!   [`ToolAccess::Alone`], or the loop forces it), a call the model asked to follow the ones before
//!   it, a call that repeats the one before it (the doom-loop count needs that result), and every call
//!   when the limit is 1. A barrier starts when every earlier call is finished and applied, and
//!   nothing after it starts before it is done.
//! * [`Queue`] orders the calls of one segment: call *j* waits for each earlier call *i* it conflicts
//!   with ([`ToolAccess::conflicts`]), and otherwise starts as soon as fewer than the limit run.
//!   Calls that are ready start in call order.
//! * [`Ordered`] gives the results back in call order whatever order they were put in.
//! * [`step_seqs`] numbers the journal steps of a turn the way the sequential loop does.
//!
//! Every function is a function of the recorded turn and the declared classes alone: how long a call
//! takes changes when it finishes, never what may run beside what.

use std::ops::Range;

use crate::access::ToolAccess;

/// One call of the turn, as the schedule sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// What the call shares with the others.
    pub access: ToolAccess,
    /// The call runs by itself whatever its access says: the loop forces it (a question, a session
    /// tool, a call that can park the run), the model asked for it, or it repeats the call before it.
    pub barrier: bool,
}

impl Entry {
    /// An entry that is a barrier for the reason of its own access only.
    #[cfg(test)]
    pub(crate) fn new(access: ToolAccess) -> Self {
        Self {
            access,
            barrier: false,
        }
    }

    fn is_barrier(&self) -> bool {
        self.barrier || self.access.is_alone()
    }
}

/// A part of the turn that is scheduled on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// One call that runs by itself: a barrier, or a lone call between two barriers.
    One(usize),
    /// Two or more calls that run together as far as their accesses allow: the indices of the calls.
    Together(Range<usize>),
}

/// The turn cut at its barriers. The segments cover every call once, in order. A `limit` of 1 makes
/// every call a segment of its own: today's behaviour.
pub(crate) fn segments(entries: &[Entry], limit: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut open: Option<usize> = None;
    let close = |out: &mut Vec<Segment>, from: usize, to: usize| match to - from {
        0 => {}
        1 => out.push(Segment::One(from)),
        _ => out.push(Segment::Together(from..to)),
    };
    for (i, entry) in entries.iter().enumerate() {
        if limit <= 1 || entry.is_barrier() {
            if let Some(from) = open.take() {
                close(&mut out, from, i);
            }
            out.push(Segment::One(i));
        } else if open.is_none() {
            open = Some(i);
        }
    }
    if let Some(from) = open {
        close(&mut out, from, entries.len());
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    Running,
    Done,
}

/// The calls of one segment waiting for their turn to start, under a limit.
#[derive(Debug)]
pub(crate) struct Queue {
    /// For each call, the earlier calls it must wait for.
    blocked_by: Vec<Vec<usize>>,
    state: Vec<State>,
    limit: usize,
    running: usize,
    stopped: bool,
}

impl Queue {
    /// A queue of calls with these accesses, in call order, running at most `limit` at once (at
    /// least one).
    pub(crate) fn new(accesses: &[ToolAccess], limit: usize) -> Self {
        let blocked_by = (0..accesses.len())
            .map(|j| {
                (0..j)
                    .filter(|&i| accesses[i].conflicts(&accesses[j]))
                    .collect()
            })
            .collect();
        Self {
            blocked_by,
            state: vec![State::Waiting; accesses.len()],
            limit: limit.max(1),
            running: 0,
            stopped: false,
        }
    }

    /// The earlier calls each call waits for: a function of the accesses alone.
    #[cfg(test)]
    pub(crate) fn blocked_by(&self) -> &[Vec<usize>] {
        &self.blocked_by
    }

    /// The calls that may start now, in call order, and mark them running: those whose conflicting
    /// earlier calls are finished, as many as the limit leaves room for. Nothing starts after
    /// [`stop`](Self::stop).
    pub(crate) fn ready(&mut self) -> Vec<usize> {
        let mut started = Vec::new();
        if self.stopped {
            return started;
        }
        for j in 0..self.state.len() {
            if self.running >= self.limit {
                break;
            }
            if self.state[j] == State::Waiting
                && self.blocked_by[j]
                    .iter()
                    .all(|&i| self.state[i] == State::Done)
            {
                self.state[j] = State::Running;
                self.running += 1;
                started.push(j);
            }
        }
        started
    }

    /// Call `i` is finished.
    pub(crate) fn finish(&mut self, i: usize) {
        if self.state[i] == State::Running {
            self.state[i] = State::Done;
            self.running -= 1;
        }
    }

    /// No call starts from now on: the segment is being cancelled or has failed.
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
    }

    /// How many calls run now.
    pub(crate) fn running(&self) -> usize {
        self.running
    }

    /// Whether every call is finished, or none is running and none can start (after
    /// [`stop`](Self::stop)).
    pub(crate) fn is_done(&self) -> bool {
        if self.stopped {
            self.running == 0
        } else {
            self.state.iter().all(|s| *s == State::Done)
        }
    }
}

/// Results put in any order and taken in call order.
#[derive(Debug)]
pub(crate) struct Ordered<T> {
    slots: Vec<Option<T>>,
}

impl<T> Ordered<T> {
    /// Room for `n` results.
    pub(crate) fn new(n: usize) -> Self {
        Self {
            slots: (0..n).map(|_| None).collect(),
        }
    }

    /// The result of call `i`.
    pub(crate) fn put(&mut self, i: usize, value: T) {
        self.slots[i] = Some(value);
    }

    /// Every result in call order, once all are in; `None` while one is missing.
    pub(crate) fn into_ordered(self) -> Option<Vec<T>> {
        self.slots.into_iter().collect()
    }
}

/// The journal seq of each call's step, as the sequential loop numbers them: `base` for the first
/// call that takes a step, one more for each next one, and `None` for a call that takes none (bad
/// arguments, an unknown tool with no source, a doom-loop refusal).
pub(crate) fn step_seqs(base: u64, takes_step: &[bool]) -> Vec<Option<u64>> {
    let mut next = base;
    takes_step
        .iter()
        .map(|&takes| {
            takes.then(|| {
                let seq = next;
                next += 1;
                seq
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::access::Use;

    fn read(scope: &str) -> ToolAccess {
        ToolAccess::reads(scope)
    }

    fn write(scope: &str) -> ToolAccess {
        ToolAccess::writes(scope)
    }

    #[test]
    fn a_turn_is_cut_at_its_barriers() {
        let entries = vec![
            Entry::new(read("files")),
            Entry::new(read("files/a")),
            Entry::new(ToolAccess::Alone),
            Entry::new(ToolAccess::Independent),
            Entry {
                access: ToolAccess::Independent,
                barrier: true,
            },
            Entry::new(ToolAccess::Independent),
            Entry::new(ToolAccess::Independent),
        ];
        assert_eq!(
            segments(&entries, 10),
            vec![
                Segment::Together(0..2),
                Segment::One(2),
                Segment::One(3),
                Segment::One(4),
                Segment::Together(5..7),
            ]
        );
        assert_eq!(segments(&[], 10), vec![]);
    }

    #[test]
    fn a_limit_of_one_makes_every_call_a_segment() {
        let entries = vec![Entry::new(read("files")); 3];
        assert_eq!(
            segments(&entries, 1),
            vec![Segment::One(0), Segment::One(1), Segment::One(2)]
        );
    }

    #[test]
    fn a_call_waits_for_the_earlier_calls_it_conflicts_with_only() {
        let accesses = vec![
            read("files/a"),
            read("files/b"),
            write("files/a"),
            ToolAccess::Independent,
            read("files"),
        ];
        let mut queue = Queue::new(&accesses, 10);
        assert_eq!(
            queue.ready(),
            vec![0, 1, 3],
            "the write of `a` waits for its read"
        );
        queue.finish(1);
        assert!(
            queue.ready().is_empty(),
            "the read of everything waits for the write"
        );
        queue.finish(0);
        assert_eq!(queue.ready(), vec![2]);
        queue.finish(2);
        assert_eq!(queue.ready(), vec![4]);
        queue.finish(3);
        queue.finish(4);
        assert!(queue.is_done());
    }

    #[test]
    fn calls_over_the_limit_queue_and_start_in_call_order() {
        let accesses = vec![ToolAccess::Independent; 5];
        let mut queue = Queue::new(&accesses, 2);
        assert_eq!(queue.ready(), vec![0, 1]);
        assert!(queue.ready().is_empty());
        queue.finish(1);
        assert_eq!(queue.ready(), vec![2]);
        queue.finish(0);
        queue.finish(2);
        assert_eq!(queue.ready(), vec![3, 4]);
    }

    #[test]
    fn a_stopped_queue_starts_nothing_and_is_done_when_the_running_calls_end() {
        let accesses = vec![ToolAccess::Independent; 3];
        let mut queue = Queue::new(&accesses, 1);
        assert_eq!(queue.ready(), vec![0]);
        queue.stop();
        assert!(!queue.is_done());
        queue.finish(0);
        assert!(queue.ready().is_empty());
        assert!(queue.is_done());
    }

    #[test]
    fn results_come_back_in_call_order() {
        let mut ordered = Ordered::new(3);
        ordered.put(2, "c");
        ordered.put(0, "a");
        assert!(Ordered::<u8>::new(1).into_ordered().is_none());
        ordered.put(1, "b");
        assert_eq!(ordered.into_ordered(), Some(vec!["a", "b", "c"]));
    }

    // ---- properties ------------------------------------------------------------------------------

    /// A random access: alone, independent, or a use of one of a few scopes that overlap in places.
    fn access_strategy() -> impl Strategy<Value = ToolAccess> {
        const SCOPES: [&str; 5] = ["files", "files/a", "files/a/x", "files/b", "commands"];
        prop_oneof![
            Just(ToolAccess::Alone),
            Just(ToolAccess::Independent),
            (0..SCOPES.len(), any::<bool>()).prop_map(|(s, w)| {
                if w {
                    ToolAccess::Uses(vec![Use::write(SCOPES[s])])
                } else {
                    ToolAccess::Uses(vec![Use::read(SCOPES[s])])
                }
            }),
            (0..SCOPES.len(), 0..SCOPES.len()).prop_map(|(a, b)| {
                ToolAccess::Uses(vec![Use::read(SCOPES[a]), Use::write(SCOPES[b])])
            }),
        ]
    }

    fn entries_strategy() -> impl Strategy<Value = Vec<Entry>> {
        proptest::collection::vec(
            (access_strategy(), prop::bool::weighted(0.2))
                .prop_map(|(access, barrier)| Entry { access, barrier }),
            0..14,
        )
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ev {
        Start(usize),
        Finish(usize),
    }

    /// Run the turn: segments one after the other, each segment's calls under its queue, with the
    /// call that finishes next chosen by `durations` (a call's duration, ties broken by call order).
    /// The log of starts and finishes, in the order they happen.
    fn simulate(entries: &[Entry], limit: usize, durations: &[u8]) -> Vec<Ev> {
        let mut log = Vec::new();
        for segment in segments(entries, limit) {
            match segment {
                Segment::One(i) => {
                    log.push(Ev::Start(i));
                    log.push(Ev::Finish(i));
                }
                Segment::Together(range) => {
                    let base = range.start;
                    let accesses: Vec<ToolAccess> =
                        range.clone().map(|i| entries[i].access.clone()).collect();
                    let mut queue = Queue::new(&accesses, limit);
                    // (finish time, call) of the running calls.
                    let mut running: Vec<(u64, usize)> = Vec::new();
                    let mut now = 0u64;
                    loop {
                        for j in queue.ready() {
                            log.push(Ev::Start(base + j));
                            running.push((
                                now + 1 + u64::from(durations[(base + j) % durations.len()]),
                                j,
                            ));
                        }
                        if queue.is_done() {
                            break;
                        }
                        // Earliest finish; the lower call index first on a tie.
                        let (pos, _) = running
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, (t, j))| (*t, *j))
                            .expect("a call is running while the segment is not done");
                        let (t, j) = running.remove(pos);
                        now = t;
                        queue.finish(j);
                        log.push(Ev::Finish(base + j));
                    }
                }
            }
        }
        log
    }

    fn position(log: &[Ev], ev: Ev) -> usize {
        log.iter()
            .position(|e| *e == ev)
            .expect("the event is in the log")
    }

    fn setup() -> impl Strategy<Value = (Vec<Entry>, usize, Vec<u8>)> {
        (
            entries_strategy(),
            1usize..=6,
            proptest::collection::vec(any::<u8>(), 1..16),
        )
    }

    proptest! {
        /// 1. Results are applied in call order for any finishing order.
        #[test]
        fn results_are_applied_in_call_order((entries, limit, durations) in setup()) {
            let log = simulate(&entries, limit, &durations);
            // Put each result as its call finishes, then read them all back.
            let mut ordered = Ordered::new(entries.len());
            for ev in &log {
                if let Ev::Finish(i) = ev {
                    ordered.put(*i, *i);
                }
            }
            let applied = ordered.into_ordered().expect("every call finished");
            prop_assert_eq!(applied, (0..entries.len()).collect::<Vec<_>>());
        }

        /// 2. Conflicting calls never overlap, and the earlier one goes first.
        #[test]
        fn conflicting_calls_never_overlap((entries, limit, durations) in setup()) {
            let log = simulate(&entries, limit, &durations);
            for j in 0..entries.len() {
                for i in 0..j {
                    if entries[i].access.conflicts(&entries[j].access) {
                        prop_assert!(
                            position(&log, Ev::Finish(i)) < position(&log, Ev::Start(j)),
                            "call {i} conflicts with {j} and must finish before it starts: {log:?}"
                        );
                    }
                }
            }
        }

        /// 3. Running calls never exceed the limit.
        #[test]
        fn running_calls_never_exceed_the_limit((entries, limit, durations) in setup()) {
            let mut running = 0usize;
            for ev in simulate(&entries, limit, &durations) {
                match ev {
                    Ev::Start(_) => running += 1,
                    Ev::Finish(_) => running -= 1,
                }
                prop_assert!(running <= limit);
            }
            prop_assert_eq!(running, 0);
        }

        /// 4. A barrier starts after all earlier calls and before all later ones.
        #[test]
        fn a_barrier_separates_the_calls_before_it_from_the_calls_after_it(
            (entries, limit, durations) in setup()
        ) {
            let log = simulate(&entries, limit, &durations);
            for (b, entry) in entries.iter().enumerate() {
                if !(entry.barrier || entry.access.is_alone()) {
                    continue;
                }
                for i in 0..b {
                    prop_assert!(position(&log, Ev::Finish(i)) < position(&log, Ev::Start(b)));
                }
                for j in b + 1..entries.len() {
                    prop_assert!(position(&log, Ev::Finish(b)) < position(&log, Ev::Start(j)));
                }
            }
        }

        /// 5. At a limit of 1 the start order is the call order, one call at a time.
        #[test]
        fn at_a_limit_of_one_calls_run_in_call_order((entries, _limit, durations) in setup()) {
            let log = simulate(&entries, 1, &durations);
            let expected: Vec<Ev> = (0..entries.len())
                .flat_map(|i| [Ev::Start(i), Ev::Finish(i)])
                .collect();
            prop_assert_eq!(log, expected);
        }

        /// 6. What may run beside what is a function of the recorded turn and the declared classes
        /// alone: the cut, and who waits for whom, never read how long a call takes, and two
        /// finishing orders of the same turn respect the same constraints.
        #[test]
        fn the_schedule_is_a_function_of_the_turn_and_the_classes(
            (entries, limit, durations) in setup(),
            other in proptest::collection::vec(any::<u8>(), 1..16),
        ) {
            prop_assert_eq!(segments(&entries, limit), segments(&entries, limit));
            for segment in segments(&entries, limit) {
                if let Segment::Together(range) = segment {
                    let accesses: Vec<ToolAccess> =
                        range.map(|i| entries[i].access.clone()).collect();
                    let (a, b) = (Queue::new(&accesses, limit), Queue::new(&accesses, limit));
                    prop_assert_eq!(a.blocked_by(), b.blocked_by());
                }
            }
            // Whatever the durations, each ran the same calls and the constraints held (2 and 4).
            for d in [&durations, &other] {
                let log = simulate(&entries, limit, d);
                prop_assert_eq!(log.len(), entries.len() * 2);
            }
        }

        /// 7. The reserved seqs equal the sequential numbering, segment by segment or all at once.
        #[test]
        fn reserved_seqs_equal_the_sequential_numbering(
            (entries, limit, _durations) in setup(),
            takes in proptest::collection::vec(any::<bool>(), 14),
            base in 0u64..1000,
        ) {
            let takes: Vec<bool> = takes.into_iter().take(entries.len()).collect();
            // The sequential loop: a counter that moves on for each call that takes a step.
            let mut counter = base;
            let sequential: Vec<Option<u64>> = takes
                .iter()
                .map(|&t| {
                    t.then(|| {
                        counter += 1;
                        counter - 1
                    })
                })
                .collect();
            prop_assert_eq!(step_seqs(base, &takes), sequential.clone());
            // Reserved a segment at a time, each from where the one before ended.
            let mut next = base;
            let mut per_segment: Vec<Option<u64>> = Vec::new();
            for segment in segments(&entries, limit) {
                let range = match segment {
                    Segment::One(i) => i..i + 1,
                    Segment::Together(r) => r,
                };
                let part = step_seqs(next, &takes[range]);
                next += part.iter().flatten().count() as u64;
                per_segment.extend(part);
            }
            prop_assert_eq!(per_segment, sequential);
        }
    }
}
