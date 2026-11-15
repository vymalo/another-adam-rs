---
description: Make the failing checks of a pull request pass.
argument-hint: "<pull request URL or repository and number>"
---
The checks of a pull request fail. Make them pass.

The pull request: $ARGUMENTS

If that names no pull request, ask which one before you start. Read what fails and why, fix the
cause in the pull request's own branch, run the checks, and push only when they pass. Say what you
changed and anything you left failing.
