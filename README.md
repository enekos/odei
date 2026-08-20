# odei

**Tiny native coding agent for the terminal, powered by Kimi.**

`odei` (Basque: the storm spirit, one of Mari's weather forms) is a single-binary coding agent.
It reads and edits files, runs commands, and searches the web from your shell — no editor
integration, no daemon, no account beyond a Kimi Coding-plan key. 2.5 MiB, no runtime deps.

Its output style aims to be closer to a Unix shell than a heavy "IDE in the terminal" TUI.

## Install

```bash
cargo install --path .
```

## Setup

Create an API key in the [Kimi Code Console](https://www.kimi.com/code), then:

```bash
odei setup          # stores the key in ~/.odei/config.json (0600)
# or:
export KIMI_API_KEY=sk-...
```

Verify with `odei doctor` — it checks the key, the profile directory, and live connectivity.

## Use

```bash
cd your_project
odei
```

The current directory becomes the workspace. Type a prompt, or `/help` for interactive commands.

One-shot, non-interactive:

```bash
odei ask "explain the changes in this repository"
```

Sessions persist automatically:

```bash
odei sessions
odei session resume last
odei session resume --id <id>
```

## Permissions

`odei` starts in **auto** mode: routine development actions run directly, while sensitive ones —
pushes, publishes, destructive shell commands, writes outside the workspace — stop for a single
approval prompt:

```
● Approval required (terminal)
Running git push origin main
 y  allow   a  always allow   n  deny
```

`a` saves a rule so the same shape of action runs unattended next time. Inspect rules with
`/allowlist`, clear them with `/permissions reset`. Other modes: `/permissions ask` (approve
everything sensitive) and `/permissions yolo` (approve nothing).

## Models

| Model | Notes |
| --- | --- |
| `kimi-for-coding` | default, all Coding plans |
| `k3` | 1M context (Moderato+) |
| `k3-256k` | 256k context (Moderato+) |
| `kimi-for-coding-highspeed` | high-speed variant (Allegretto+) |

Switch at runtime with `/model k3`, or persistently via `ODEI_MODEL`.

## Tools

`read_file`, `code_outline`, `write_file`, `edit_file`, `list_files`, `glob_files`,
`grep_files`, `delete_file`, `rename_file`, `copy_file`, `create_folder`, `file_info`,
`semantic_search`, `terminal`, `read_tool_result`, `web_fetch`, `web_search`.

### Structure without reading

A built-in structural scanner (no tree-sitter, no grammars, nothing to install) recovers the
declaration skeleton of Rust, TypeScript/JavaScript, Python, Go, and C-family sources: every
function, method, type, class, and constant, with its signature, nesting, and line span. It
strips comments and strings while tracking brace depth (indentation for Python), so a string
containing `fn fake() {` fools neither the depth nor the outline.

It surfaces in three places, all automatic:

- **`code_outline`** maps a file — or a whole directory, one skeleton per file — so the model
  picks a `start_line` instead of reading top to bottom. A 600-line file costs ~35 lines.
- **Truncated `read_file` output** ends with a map of the declarations in the part that didn't
  fit, so the next read jumps rather than pages.
- **`grep_files` hits** carry the declaration they sit in (`[in pub fn read_file]`), so a match
  is locatable without another read.

### The terminal is a real terminal

Commands run on a pseudo-terminal, not a pipe, so programs take the same code path they would
for a person at a prompt — colour, progress bars, and anything that checks `isatty` all behave.
Output comes back with escape sequences removed and rewritten progress lines collapsed to their
final state, so a spinner costs one line instead of ten thousand.

`exec` runs to completion and returns combined output plus the exit code. `start` leaves a
session alive and hands back an id for `read`, `write`, `wait`, `signal`, `resize`, and `close`,
which is how a dev server, a REPL, or anything that wants typing gets driven. Signals reach the
whole process group, so a shell's children go down with it. Captured commands run with
`PAGER=cat` and `GIT_TERMINAL_PROMPT=0` — a command that stops for a pager would otherwise just
burn its timeout.

## Markdown, rendered as it streams

The model writes markdown, so the shell reads markdown — while the answer is still
arriving, not after it lands. Headings and `**strong**` become weight, `` `code` `` a
panel, lists get real bullets and hanging indents, `- [x]` a checkbox, block quotes a
bar, and tables are measured, aligned, and folded to fit:

```
  Tool       │    Cap │ Off-transcript │ Notes
  ───────────┼────────┼────────────────┼──────────────────────────────────
  read_file  │   8 KB │      yes       │ truncation adds a remainder map
  terminal   │ 256 KB │      yes       │ pty output, escapes stripped and
             │        │                │ progress lines collapsed
```

Column widths come from the content, `:---:` and `---:` are honoured, and a table too
wide for the terminal has its widest columns squeezed and its cells folded rather than
wrapping into nonsense.

Three things it takes care to get right:

- **Prose reflows.** A model hard-wraps its paragraphs at its own measure; a source
  newline is treated as the space it stands for, so paragraphs and list items rewrap to
  *your* width instead of breaking twice.
- **Unfinished markup is not guessed at.** Output pauses at an opening marker until its
  closer arrives — so `**bold**` never bolds the rest of a paragraph, and a marker that
  turns out to be alone (`*.rs`, `2 * 3`, `snake_case`) prints as itself.
- **It stays a stream.** Only two things are ever buffered: the start of a line, until
  its shape is certain, and a table, until its last row is in. Everything else appears
  word by word.

Code blocks are left exactly as written — no markup is interpreted inside them — and
links keep their destination, because a terminal link you cannot copy is worse than one
you can see. `NO_COLOR`, a redirected stdout, or `ODEI_MARKDOWN=off` turns the whole
thing off: piped output is byte-for-byte what the model produced.

## Inspecting a tool call

Every completed activity line carries a handle:

```
● 3 tool calls · 2 reads · 1 command
  ├ Read src/agent.rs  #1
  ├ Searched TODO  #2
  └ Ran cargo test --lib ✗  #3
```

`/calls` lists the session's calls and lets you **click one**; `/call 3` goes
straight there. Either way you get the whole account of that step — the
command that reproduces it, the arguments, and the complete output, including
the part the model never saw because it was truncated for the transcript:

```
odei call #3  ·  terminal  ·  failed  ·  2.41s  ·  2026-08-20 14:00:09
Ran cargo test --lib
cwd /Users/you/proj

── Ran this ────────────────────────────────────────────────────────────
cd /Users/you/proj
PAGER=cat GIT_PAGER=cat GIT_TERMINAL_PROMPT=0 /bin/zsh -lc 'cargo test --lib'

── Arguments ───────────────────────────────────────────────────────────
{
  "action": "exec",
  "command": "cargo test --lib"
}

── Output · 8411 bytes · complete · the model saw an 8 KB preview of this
running 214 tests
...
```

For the `terminal` tool that block is the command that actually ran, env and
shell flags included. For everything else it is an honest stand-in — the
`grep -rn` or `sed -n` that would do the same thing — labelled as
approximate, because odei did the work natively.

It opens in a **real side pane** when the terminal can split itself: tmux,
Zellij, WezTerm, and kitty are each detected from their own environment
variable, so scrollback, search, and copy are the ones you already use. With
no multiplexer it opens in your pager. The rendered report is a plain-text
file under `~/.odei/calls/` and its path is always printed, so it pipes and
pastes like anything else.

Mouse reporting is armed only while the picker is on screen. The rest of the
time — and in the pane itself — dragging selects text exactly as it always
did.

## Context management

Two mechanisms keep a long session inside the model's window.

**Large results go off-transcript.** A tool result over 8 KB is written to
`~/.odei/tool-results/` and the conversation keeps only its head, its tail, and a handle. The
model reaches the rest with `read_tool_result`, by byte range or by searching it. A 170 KB build
log costs about 8 KB of context until something in the middle actually matters.

**Older turns get summarized.** At 75% of the window, odei asks the model to compact the earlier
history into a dense brief — intent, decisions, paths touched, commands and their results, what
is verified, what is still open — and replaces those turns with it. `/compact` does the same on
demand. The cut is only ever taken at a real user turn, so a tool call is never separated from
its result.

## Project instructions

`odei` reads `AGENTS.md` from the workspace root and `~/.odei/AGENTS.md` globally, and passes
both along as project instructions. Whatever you ask for in conversation outranks them, and
when two of them disagree the one scoped nearest the files being touched wins.

## Environment

| Variable | Purpose |
| --- | --- |
| `KIMI_API_KEY` | Kimi Coding-plan key |
| `ODEI_API_KEY` | alternative key variable |
| `ODEI_MODEL` | model override |
| `ODEI_BASE_URL` | endpoint override (default `https://api.kimi.com/coding`) |
| `ODEI_PERMISSIONS` | `ask` / `auto` / `yolo` |
| `ODEI_MAX_AGENT_STEPS` | agent loop step cap (default 120) |
| `ODEI_THEME` | `light` / `dark` (auto-detected otherwise) |
| `ODEI_MARKDOWN` | `off` to print the model's markdown source verbatim |
| `NO_COLOR` | disable styling |

Profile data lives in `~/.odei`: `config.json`, `permissions.json`, `sessions/*.jsonl`,
`tool-results/`, `calls/`, `usage.jsonl`, `history`. Stored tool results and call
journals are pruned after 7 days.

## Develop

```bash
cargo build --release
cargo test
cargo clippy
```

Module map: `provider.rs` (Kimi SSE client) · `agent.rs` (tool loop) · `tools/` (registry +
implementations, incl. `outline.rs`) · `ui.rs` (interactive shell) · `markdown.rs` (streaming
markdown renderer) · `permissions.rs` · `session.rs` · `compact.rs` (history summarization) ·
`calls.rs` (call journal + reports) · `inspect.rs` (picker + side pane) · `context.rs` (system
prompt + runtime context) · `theme.rs`.

## Not implemented

MCP servers, skills, subagents, vision/image input, undo of file mutations, and an
LLM-based permission reviewer (a static classifier handles auto mode instead).

## License

Apache-2.0.
