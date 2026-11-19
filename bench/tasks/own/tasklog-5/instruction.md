# Due dates for tasks

tasklog has no way to say when something must be done. Add due dates, end to end:

1. `tasklog add <title> --due YYYY-MM-DD` gives the new task a due date. A value that is not a real calendar date
   (`2026-02-30`, `tomorrow`) is a usage error (exit 2, the message names the value) and nothing is written.
2. The file keeps it as a `due:<date>` word after the tags and before `done:`, for example
   `[x] 1 Buy milk #home due:2026-11-01 done:2026-11-02`. Reading a file with a `due:` word that is not a real date is
   an error that names the line, like the other errors of the format. A task without a due date is written exactly as
   today, and marking a task done keeps its due date. A parsed task exposes the date as its `due` property.
3. `list` shows it after the tags: `  1 [ ] Pay rent #home (due 2026-12-01)`.
4. `list --overdue` shows only the open tasks whose due date is before today (today is the date of the `now` that
   `run` is given, in UTC, as `done` already uses). A task due today is not overdue, a done task never is. It combines
   with `--tag` and `--sort`.
5. `list --sort due` sorts by due date, earliest first, tasks without a due date last, and by id when dates are equal.

Update the README. The existing tests must keep passing (`npm test`).
