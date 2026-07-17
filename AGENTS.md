# Agent instructions for daw-ai

This file tells coding agents how to work productively in this repository.

## charter.md

Treat the charter.md file as the authoritative project document from the user. NEVER modify it yourself,
unless the user explicitly instructs you to. It should only be modified by the user. Your job is
to implement the project and features that they describe in this document.

## Before completing codebase-changing work

**Run `just test` and confirm it passes after making any change that can affect the codebase
in the current working directory.**
This target runs the `pre` recipe first, which checks Rust formatting, runs Clippy with warnings
denied, and checks the JavaScript syntax. It then runs the Rust test suite, builds the server,
and exercises the core UI workflows in headless Chrome or Chromium.
If any of those fail, fix the underlying issue; do not bypass checks.

## Style guide

- Comments should be brief and focus on important invariants, architectural details, or other
  long-term relevant information. They should not contain minor implementation details of the current
  commit.

## Tests

When adding new features, add tests, but aim for high code coverage and important integration
tests without adding too many lines of new test code. Expanding a logically related existing test is
often a good way to achieve coverage without bloating the suite.

## Git commits

1. Git commits should use your human's name and email address for authorship. Add `Assisted-by:`
   and your agent name at the end of the commit message. Use the same style as the
   [Linux Kernel's coding assistant guidelines](https://github.com/torvalds/linux/blob/master/Documentation/process/coding-assistants.rst).
2. Make one commit per feature or bug fix when opening a PR. Multiple commits or fixup commits should
   not be merged to the default branch.
