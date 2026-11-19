# Support days and milliseconds in durations

`parseDuration` and `formatDuration` only know hours (`h`), minutes (`m`) and seconds (`s`). We need two more units:

* `d`, a day (24 hours), larger than `h`;
* `ms`, a millisecond, smaller than `s`.

The existing rules apply to the new units too: each unit at most once, larger units first, no spaces, anything else is a
`SyntaxError`. Examples:

| Input | `parseDuration` |
|---|---|
| `2d` | `172800000` |
| `1d12h` | `129600000` |
| `250ms` | `250` |
| `1m30s250ms` | `90250` |
| `10ms1s` | `SyntaxError` (out of order) |

`formatDuration` must use the new units as well, so that it no longer drops what is below a second: `90061001` formats
as `1d1h1m1s1ms`, `250` as `250ms`, `1500` as `1s500ms`, and `0` is still `0s`. Update the README's list of units.
Everything else keeps working and `npm test` passes.
