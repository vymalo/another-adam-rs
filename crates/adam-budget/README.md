# adam-budget

The resource budget and the admission port of adam-rs: what a process may use at once, and the
permit that gives a share back. A pure crate: no I/O, no async of its own (a trait method is
`async`, and it performs no I/O), no clock of the wall.

The decision is
[ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md),
slice 2.

## What is in it

| Item | Purpose |
|---|---|
| `Budget` | One ceiling: runs, tool calls, processes, git processes, MCP servers, model calls, an optional token rate, a command's memory and CPU, the disk budget, the disk reserve, and the share one conversation may hold. `#[non_exhaustive]`, built with `Budget::builder()` from `Budget::UNLIMITED`. |
| `Budget::lower(&self, &Budget)` | **The only way to combine two budgets.** Field by field the smaller value, so a folder or a person lowers the deployment's ceiling and never raises it. |
| `Ratio` | A ratio in ten-thousandths (no `f64`): `1/2` is `Ratio::HALF`. `apply_u64`, `apply_u32`. |
| `DiskReserve` | A fixed number of bytes or a ratio of the disk. |
| `Pool` | One kind of work with its own ceiling: `Runs`, `ModelCalls`, `ToolCalls`, `Processes`, `Git`, `McpServers`, `CommandMemory` (its unit is a MiB). Closed enum. |
| `Admission` | The port: `async fn admit(&self, ask: Ask) -> Result<Permit, Refused>`, `fn snapshot(&self) -> Usage`. No driver type appears in it. |
| `DynAdmission` | `Arc<dyn Admission>`. |
| `Ask` | One question: the pool, the conversation, how many units, the tokens a model call is estimated to use, and how long it may wait (zero refuses at once). |
| `Permit` | A share of a pool, **released when it is dropped**, so a cancelled, failed or panicking holder frees it. A permit is never released twice. |
| `Refused` | Why an ask got no permit, with the class a caller reacts to through `Classify`: `Busy`/`TokensPerMinute` are `RateLimited` (park the run and ask again at `retry_after`), `DiskFull` is `Rejected` (ask the person), `TooLarge` is `Invalid` (the same ask never fits). No new `ErrorClass`. |
| `Budget::ceiling(Pool)` | The pool's ceiling as a report reads it: `Limit::Unlimited` for a field the budget leaves at `Budget::UNLIMITED`'s value, `Limit::AtMost(n)` otherwise. `Budget::limit(Pool)` stays the numeric ceiling a semaphore needs. |
| `Limit` | A pool's ceiling in a report: `Unlimited`, or `AtMost(u64)`. **An absent limit is not a limit of zero**: `AtMost(0)` is a pool a deployment closed on purpose. `at_most()`, `is_unlimited()`, `Display` (`unlimited`). |
| `DeviceRules`, `Constraint`, `LOW_BATTERY_PERCENT` | The fixed rule table of ADR 0065 decision 8, pure: `DeviceRules::constraint(&DeviceReadings)` maps the host's readings to a `Constraint` (`max_runs`, never below one; `hold_new_steps`; `no_background_commands`; `ask_before_large_fetch`). One switch per rule (`low_battery`, `hot`, `metered`), all on by default; `DeviceRules::OFF` turns every one off. This crate depends on `adam-device-state` for the readings, which has no dependency of its own. |
| `Throttled<A: Admission>` | An `Admission` that lowers `A` by the live readings of a `DynDeviceState` at every ask: on a low battery a `Pool::Runs` ask waits for the runs already admitted (one at a time); on a hot device `Pool::ModelCalls` and `Pool::ToolCalls` asks wait, then `Busy`. `no_background_commands` and `ask_before_large_fetch` are computed but **not enforced**, because an `Ask` does not say what a process is or fetches. `set_rules` swaps the switches live. |
| `Usage`, `PoolUsage`, `TokenUsage` | What is held now, pool by pool: each `PoolUsage` has its `Limit`, what is held, and how many asks wait. It names no person, conversation or run. |

## Sizes and shares

`parse_bytes("2GB")` (binary units), `parse_millicores("1.5")` or `("500m")`, `default_command_memory(ram)` (a quarter
of the RAM, at most 8 GB; 2 GB when the RAM is unknown) and `default_command_cpu(cores)` (half the cores, at least
one): how `COMMAND_MEMORY_MAX` and `COMMAND_CPU_MAX` are read and defaulted. The machine's RAM and core count are
arguments, so the crate stays free of I/O. `Budget::command_memory` and `command_cpu` are what **one command** may
use; the commands' pool is `Pool::Processes`, and `Pool::CommandMemory` is not asked by the process pool.

## The two rules a caller relies on

1. **A ceiling is lowered, never raised.** Build it once at the deployment; a folder's or a
   person's budget is combined with `Budget::lower`.
2. **A share is given back by dropping its permit.** Never call a release by hand: there is none.

`Budget::UNLIMITED` is the identity of `lower`: `UNLIMITED.lower(&x) == x` and
`x.lower(&UNLIMITED) == x`, so a deployment states only what it bounds.

The disk reserve is the one field `lower` tightens the other way: the stricter reserve wins, since
a lower budget must not keep less free. A fixed reserve and a ratio cannot be compared without the
disk, so the ratio is kept (it follows the disk).

## Implementations

* [`adam-admission-local`](../adam-admission-local/README.md): `LocalAdmission`, tokio semaphores
  in one process.
* `StoreAdmission` (counted in the run store, several replicas) is decided later, in the component
  that needs it.

Every implementation passes [`admission_conformance!`](../adam-admission-testkit/README.md).

## Tests

`cargo test -p adam-budget` runs the unit tests of the budget, the refusals and the permit:
`unlimited_is_the_identity_of_lower`, `lower_takes_the_smaller_of_every_count_and_rate`,
`a_ratio_applies_without_overflow`, `a_pool_limit_is_its_field_in_its_unit`,
`every_pool_has_a_setter`, `every_refusal_has_a_class_and_no_new_one`,
`a_permit_releases_once_and_frees_its_share_on_drop`, `a_usage_adds_up_by_pool` and
`a_pool_names_itself_for_a_metric`, `no_limit_is_not_a_zero_limit` (ADR 0065 decision 9: a
budget that sets nothing reports `Unlimited` for every pool, a pool set to zero stays `AtMost(0)`)
and `a_limit_names_itself`, plus `the_rule_table_lowers_by_each_reading_and_never_below_one_run`
(the pure table, each switch, the floor of one). The behaviour of `Throttled` over a real admission
is in [`adam-admission-local`](../adam-admission-local/README.md)'s `tests/device.rs`.
