# odei

**Tiny native coding agent for the terminal, powered by Kimi.**

`odei` (Basque: the storm spirit, one of Mari's weather forms) is a single-binary coding agent.
It reads and edits files, runs commands, and searches the web from your shell — no editor
integration, no daemon, no account beyond a Kimi Coding-plan key. 2.5 MiB, no runtime deps.

Its output style aims to be closer to a Unix shell than a heavy "IDE in the terminal" TUI.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/enekos/odei/master/install.sh | sh
```

Puts a single binary in `~/.local/bin` — macOS on Apple silicon or Intel, Linux on x86-64 or
arm64. No sudo, no runtime dependencies, nothing written outside that directory and `~/.odei`.

Piping a script into a shell is worth being careful about, so here is exactly what it does: works
out which release asset fits your machine, downloads it over HTTPS, checks it against the SHA-256
published in the same release, and refuses to install anything that does not match. What it
downloads is a tarball, never more shell code. To read it first — the better habit — do that:

```bash
curl -fsSL https://raw.githubusercontent.com/enekos/odei/master/install.sh -o install.sh
less install.sh
sh install.sh
```

Options work as flags or environment variables: `--version v0.1.0`, `--dir /usr/local/bin`,
`--force`. Through a pipe they need a separator — `| sh -s -- --dir ~/bin`.

Release artifacts also carry signed build provenance, so you can confirm a tarball came from this
repository's release workflow rather than from someone else:

```bash
gh attestation verify --repo enekos/odei odei-v0.1.0-aarch64-apple-darwin.tar.gz
```

Prefer to build it yourself:

```bash
cargo install --git https://github.com/enekos/odei --locked odei   # any platform, from source
cargo install --path .                                             # from a clone
```

## Update

```bash
odei upgrade           # check for a newer release and install it
odei upgrade --check   # only say whether one exists
```

`upgrade` runs the installer published with the release it is installing, so installing and
updating share one code path; re-running the `curl` line does the same thing. Either way it stops
early when you are already current.

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
`semantic_search`, `terminal`, `worktree`, `read_tool_result`, `web_fetch`, `web_search`.

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

### Worktrees, by name rather than by path

The `worktree` tool lets the agent work in your other checkouts without being told where they
are. A query is a fuzzy directory name and `query@branch` names a worktree inside it, with the
branch matched exact name first, then unique prefix, then unique substring — `odei@swift` finds
`odei-worktrees/swift-ui`. An ambiguous fragment lists the candidates instead of guessing.

```
resolve   the directory, and nothing else — feed it to read_file or terminal's cwd
list      every worktree of the resolved repository, with its branch
create    make a missing worktree, cutting the branch from the default branch
track     the same for a branch that only exists on the remote, fetching it first
run       one command in the resolved directory, exactly like terminal's exec
merged    which worktrees have landed, and why the rest haven't
clean     retire the landed ones: remove, delete the branch, prune
```

Resolution goes through [`zz`](https://github.com/enekos/zz) when it is installed, which ranks
the directories you actually visit and, failing that, scans the linked worktrees of every
repository it knows — so a branch fragment finds a checkout nobody has opened yet. Without `zz`
the query has to be a path, and everything else works the same, because it is git underneath.
New worktrees land in `zz`'s layout, a sibling `<repo>-worktrees/<branch-slug>`, honouring
`ZZ_WORKTREE_DIR` and `ZZ_WORKTREE_BASE`.

`clean` is the one action that deletes, so it is deliberately timid. A branch counts as landed
when it is an ancestor of the base branch, or when `gh` reports a merged pull request for it —
which is how squash merges, invisible to git, are caught. It never touches the main checkout,
the directory the session is working in, a worktree with uncommitted changes, or a branch with
commits that haven't landed. `merged` is the same pass without the removals: run it first.
`force` overrides the uncommitted-changes guard and nothing else — unmerged work is never
removable.

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
| `ZZ_WORKTREE_DIR` | where the `worktree` tool creates worktrees (default: a sibling `<repo>-worktrees`) |
| `ZZ_WORKTREE_BASE` | base ref for branches it cuts (default: `main`, then `master`) |
| `NO_COLOR` | disable styling |

Profile data lives in `~/.odei`: `config.json`, `permissions.json`, `sessions/*.jsonl`,
`tool-results/`, `calls/`, `usage.jsonl`, `history`. Stored tool results and call
journals are pruned after 7 days.

## macOS app

`ui/` is a SwiftUI front end. It does not reimplement the agent — it runs `odei serve` as
a child process and speaks NDJSON to it, so the loop, the tools, the permission gate, the
sessions and the call journal are the same code the terminal uses.

```bash
cargo install --path .        # the app looks in ~/.cargo/bin, ~/.local/bin, homebrew
ui/build-app.sh               # writes ui/Odei.app — no Xcode needed, just Swift
open ui/Odei.app
```

`⌘O` picks the workspace, `⌘N` starts a session, `⌘↩` sends, `⌘.` interrupts. Permission
prompts appear above the composer with `y` / `a` / `n` bound to Allow / Always / Deny.
Every finished tool line carries its `#N`; clicking it opens the same full report that
`/call N` shows in the terminal, in an inspector pane. The sidebar lists this session's
calls and every saved session — picking one resumes it. `ODEI_BIN` overrides binary
discovery.

### serve

```
odei serve [--workspace <dir>] [--resume last|<id>]
```

One JSON object per line, both directions, nothing else on stdout.
In: `prompt`, `approve`, `cancel`, `compact`, `sessions`, `calls`, `call`, `model`,
`mode`, `exit`. Out: `ready`, `state`, `history`, `waiting`, `text`, `text_end`, `group`,
`tool`, `approval`, `notice`, `turn_end`, `sessions`, `calls`, `call`, `error`, `fatal`.

`cancel` and `approve` are handled on the stdin reader thread because they have to land
while the agent thread is blocked; everything else is queued for the loop.

## Develop

```bash
cargo build --release
cargo test
cargo clippy

python3 ui/checks/serve-protocol.py target/debug/odei   # serve, against a mock endpoint
swift run --package-path ui OdeiChecks                  # the app's model layer
```

Module map: `provider.rs` (Kimi SSE client) · `agent.rs` (tool loop) · `tools/` (registry +
implementations, incl. `outline.rs` and `worktree.rs`) · `ui.rs` (interactive shell) · `serve.rs` (NDJSON
front-end protocol) · `permissions.rs` · `session.rs` · `compact.rs` (history
summarization) · `calls.rs` (call journal + reports) · `inspect.rs` (picker + side pane) ·
`context.rs` (system prompt + runtime context) · `theme.rs`.

The Swift side splits into `OdeiCore` (protocol, process, transcript) and `OdeiUI`
(views), so the model layer can be checked without a window — XCTest ships with Xcode,
which this app deliberately does not require, so `OdeiChecks` is a plain executable that
exits non-zero on failure.

## Not implemented

MCP servers, skills, subagents, vision/image input, undo of file mutations, and an
LLM-based permission reviewer (a static classifier handles auto mode instead).

## License

Apache-2.0.
