# Troubleshoot by error class: evidence

For people: [Troubleshoot by error class](../../../docs/guides/troubleshoot-by-error-class.md). That page decides; this one gives where each row comes from.
Checked at `257175c` on 2026-11-26. The classes, the A2A codes and the exit codes are in [Errors](../../../docs/reference/errors.md); the rules to follow when you
add an error type are on its agent page, [Errors: rules to check](../reference/errors.md).

| Claim | Where it is true |
|---|---|
| the class decides retry, alert, the A2A error and the exit code | `docs/reference/errors.md` "Classes"; `crates/adam-error` |
| a runtime error becomes a `BackendError` by class: `NotFound`, `Invalid` and `Rejected`, the three retryable ones, the rest `internal` | `crates/adam-a2a-runtime/src/backend.rs:1242-1261` (`map_err`) |
| the client gets a fixed sentence, the cause chain goes to the log | `docs/reference/errors.md` "What each class decides" |
| exit: `Invalid` 78, the three retryable 69, `Corrupt` and `Internal` 70, the rest 1; an OS error 71; a panicked task 70 | `crates/adam-service/src/exit.rs:96-107` |
| a `Connect` or `Migrate` failure takes its cause's class | `crates/adam-service/src/exit.rs:91-92` |
| the retry policy: five tries, one second doubling to a minute | `crates/adam-runtime/src/retry.rs:141-148` |
| a commit that loses a race is tried up to 16 times | `crates/adam-runtime/src/runtime.rs:38` (`MAX_COMMIT_RETRIES`) |
| a store error while stepping: `Corrupt` and `Invalid` fail the run, the rest keep the lease | `docs/reference/errors.md` "Retry" |
| a refused ask for a resource is `RateLimited` (`Busy`, `TokensPerMinute`), `Rejected` (`DiskFull`) or `Invalid` (`TooLarge`) | `docs/reference/errors.md` "Admission refusals" |
| a caller with no bearer gets `401`, any other route | `docs/guides/deploy-the-coder.md` "Verify" |
| `CreateContainerConfigError` for a missing `ExternalSecret` property | `docs/guides/deploy-the-coder.md` "Verify" |
| `Unsupported` has no enum that maps to it | `docs/reference/errors.md` "Classes" (its row) |

*Unverified:* the BSD `sysexits.h` numbers, as the errors page says; and that a run whose step ends `NotFound`, `Rejected` or `Unauthenticated` is always failed (the retry
section says "any other class fails the run"; in the coder a permanent tool failure is an error result the model sees).
