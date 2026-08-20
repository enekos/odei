# evals

Behavioural evals for odei: does the agent actually do what the system prompt
tells it to do. Run them with `odei eval` from the repo root.

```
odei eval                  every case
odei eval no-push verify   cases whose name contains either fragment
odei eval --list           names and what each one is for
```

Each case runs the real agent loop against a throwaway workspace under
`~/.odei/eval/<timestamp>/<case>/`, which is left behind for inspection and
pruned after seven days. Results land in `evals/results/<timestamp>.json`, and
the tool call journal for every run is in `~/.odei/calls/`, so `odei` can be
pointed at a failure afterwards to see exactly what happened.

Approvals are always denied. In `auto` mode only the sensitive-command list
reaches the gate, so a case that wants to watch a *routine* write get blocked
asks for `"permissions": "ask"`.

## A case

```
evals/cases/verify-after-change/
  task.md        the prompt, sent verbatim as the user turn
  expect.json    assertions
  fixture/       optional; copied into the workspace
  setup.sh       optional; runs in the workspace after the copy (git init,
                 generating a large file, anything awkward to check in)
```

Assertions are ordinary code reading three things: the call journal, the final
assistant text, and the resulting file tree. Unknown fields in `expect.json`
are an error, so a typo fails loudly instead of quietly passing.

| field | meaning |
|---|---|
| `about` | one line on what the case tests; shown by `--list` |
| `permissions` | `ask`, `auto` (default) or `yolo` |
| `max_steps` | agent-step cap; hitting it is a failure |
| `max_tool_calls` | budget for the whole turn |
| `tools_used` | every one of these must have been called |
| `tools_used_any` | at least one of these |
| `tools_not_used` | none of these |
| `order` | `[["read_file", "edit_file"]]` — some call of the first precedes some call of the second |
| `max_chars_before_tools` | how much it may say before its first tool call — a preamble is fine, an answer from memory is not |
| `command_matches` | regex; some `terminal` command must match |
| `command_succeeds` | as above, and at least one matching run must not be an error |
| `command_not_matches` | no `terminal` command may match |
| `approval_requested` | these tools must have hit the approval gate |
| `files` | per path: `contains`, `not_contains`, `unchanged`, `exists` |
| `no_writes_outside` | no path-tool mutation may resolve outside the workspace (shell writes are not covered) |
| `text_matches` / `text_not_matches` | regex over the final answer |
| `judge` | a rubric the model scores PASS/FAIL, for things code can't see |

`judge` gets the task, the answer, and the list of what actually ran — which
is what makes it able to catch a claimed check that never happened.

## Comparing prompts

`ODEI_SYSTEM_PROMPT_FILE=<path> odei eval` swaps the whole system prompt for
the contents of that file. The path used is recorded in the results JSON, so
two runs can be diffed against each other.

## Reading a failure

The failure line names the assertion, not the fix. Two things worth checking
before believing it:

- Is the assertion right? An eval that contradicts the system prompt will
  fail on correct behaviour. `max_chars_before_tools` exists because the
  first version of that check ("no text before any tool call") failed a run
  where the model did exactly what the prompt asks and said one line first.
- Was it luck? A single case is one sample. Re-run before rewriting a prompt
  section on the strength of one red line.
