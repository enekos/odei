# odei

**Tiny native coding agent for the terminal, powered by Kimi or Gemini.**

`odei` (Basque: the storm spirit, one of Mari's weather forms) is a single-binary coding agent.
It reads and edits files, runs commands, and searches the web from your shell — no editor
integration, no daemon, no account beyond a Kimi Coding-plan key (or a Gemini API key). 2.5 MiB,
no runtime deps.

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

For Gemini, create a key in [Google AI Studio](https://aistudio.google.com/apikey), then:

```bash
odei setup gemini   # stores the key alongside the Kimi one
# or:
export GEMINI_API_KEY=AI...
export ODEI_MODEL=gemini-2.5-flash
```

The provider follows the model id: any `gemini-*` model talks to the Gemini API, everything
else to Kimi. `ODEI_PROVIDER=kimi|gemini` overrides the inference for proxies with
unrecognizable model ids.

Verify with `odei doctor` — it checks the key, the profile directory, and live connectivity.

## Use

```bash
cd your_project
odei
```

The current directory becomes the workspace. Type a prompt, or `/help` for interactive commands.

*odei* is Basque for cloud, so it opens as one: the wordmark condenses out of drifting
mist — a noise cloud blowing across the field while the letters thicken to solid — in
about four tenths of a second, seeded from the clock, so no two launches look alike.

```
      ██████      ████████    ██████
    ██      ██  ██      ██  ██      ██  ██
    ██      ██  ██      ██  ██████████  ██
    ██      ██  ██      ██  ██          ██
      ██████      ████████    ██████    ██
    ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁
    v0.1.0 · kimi-for-coding · your_project
```

`/splash` plays it again. It is grayscale like the rest of the shell, it redraws in place
rather than clearing the screen, Ctrl+C cuts to the settled frame, and it gets out of the
way on its own: a pipe, `NO_COLOR` or a window too narrow for the wordmark prints the
one-line greeting instead, a window too short to redraw draws the settled frame without
animating, and `ODEI_SPLASH=off` (or `=static`) makes either the rule.

One-shot, non-interactive:

```bash
odei ask "explain the changes in this repository"
```

Anything piped in joins the prompt, which makes `ask` an ordinary filter:

```bash
git diff | odei ask "review this"
cargo test 2>&1 | odei ask "why did this fail?"
```

`--output-format` decides who is reading. `text` (the default) renders for a terminal; `json`
answers with one object — the reply, the session id, the model, what ran, what it cost; and
`stream-json` writes the same NDJSON events [`odei serve`](#serve) speaks, so a script can watch
a turn instead of waiting for it.

```bash
$ echo 'Reply with exactly: OK' | odei ask --output-format stream-json
{"event":"waiting","step":1}
{"delta":"OK","event":"text"}
{"event":"text_end"}
{"event":"step","step":1,"input_tokens":4300,"output_tokens":17,"ms":4767,"tool_calls":0}
{"event":"result","ok":true,"result":"OK","session_id":"20260827-084206-18e3", …}
```

Both machine formats exit non-zero on a failed turn and report the error in the payload rather
than on stderr. Nobody is there to answer an approval prompt, so anything sensitive enough to
stop for one is denied instead of hanging — `json` names it in `notices`, `stream-json` emits the
`approval` event and then denies it. Give an unattended run a saved rule, or
`ODEI_PERMISSIONS=yolo`.

Sessions persist automatically:

```bash
odei sessions
odei session resume last
odei session resume --id <id>
```

## Three prefixes at the prompt

A line that starts with `@`, `!` or `#` is handled before the model sees anything.

**`@<path>` attaches it.** A file comes in whole; a directory comes in as its `code_outline`
map, or a plain listing if nothing in it parses. The paths are read as of that turn and the
model is told it has already read them, so it does not spend a tool call re-reading. Quote a
path with spaces — `@"my notes.md"` — and note that `eneko@example.com` is left alone: `@` only
counts at the start of a word. Ten attachments and 64 KB per message; past that the rest is cut
with a pointer to `read_file`.

```
› explain @src/mentions.rs against @src
attached @src/mentions.rs (file, 151 lines) · @src (map, 84 lines)
```

**`!<command>` runs it yourself.** It skips the permission gate — you are the one asking — but
it is journalled like any other call, so `/call N` replays it in full. The command and its
output ride along with your *next* message rather than starting a turn, which means you can
look at something first and then ask about it without pasting.

```
› !cargo test 2>&1 | tail -5
└ Ran cargo test · 0.9s · 131 passed
  test result: ok. 131 passed; 0 failed
```

**`#<note>` remembers it.** It appends the note as a bullet to `AGENTS.md`, asking first whether
it belongs to this project or to every workspace — `p` writes `./AGENTS.md`, `g` writes
`~/.odei/AGENTS.md`. Both are read back as project instructions on every turn, so a `#` note is
in force from the next message on.

## Your own commands

A markdown file in `.odei/commands/` is a slash command. `deploy-check.md` becomes
`/deploy-check`, and its body is sent as the prompt:

```markdown
---
description: check the release is safe to tag
---

Run the tests and clippy, then read the diff against the last tag and tell me
what would ship in $ARGUMENTS.
```

`$ARGUMENTS` is everything typed after the command, `$1`–`$9` are its words. A body with no
placeholder gets the arguments appended to it instead, so a command never silently drops what
you typed. Project commands live in `.odei/commands/`, personal ones in `~/.odei/commands/`, and
a project name shadows a personal one. `/help` lists both under the built-ins.

Tab completes commands at the start of a line — built-in and your own, from the same table
`/help` prints — and paths after `@`, directories first.

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
| `gemini-2.5-flash` | Gemini default (GEMINI_API_KEY) |
| `gemini-flash-latest` | newest Gemini Flash (GEMINI_API_KEY) |
| `gemini-pro-latest` | newest Gemini Pro (GEMINI_API_KEY) |

Switch at runtime with `/model k3` — switching to a `gemini-*` model switches provider, key,
and base url with it — or persistently via `ODEI_MODEL`.

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

## What the agent is doing, at three depths

An activity line says what ran, what the arguments added to that, what came
back, how long it took, and the handle that reopens it — and a call that
changed a file draws the change:

```
● 3 tool calls · 1 read · 1 edit · 1 command
  ├ Read src/agent.rs · lines 120–319  142 lines  #1
  ├ Edited src/ui.rs  +8 −3  #2
  │  41    fn finish_tool_line(&mut self) {
  │  42 -     if self.tool_line_open {
  │  42 +     if self.tool_line_open && !self.quiet {
  │  43        println!();
  └ Ran cargo test --lib ✗  exit 101 · test result: FAILED. 1 failed  2.4s  #3
    … 214 earlier lines · /call 3
    thread 'ui::tests::a_narrow_window' panicked at src/ui.rs:1104
    test result: FAILED. 121 passed; 1 failed
```

A failure always says why on its own line — a command by its exit code and
its last word, a tool by its message — and shows the end of its output, the
part that explains it, without being asked. Everything else is one line.

Two commands move the whole shell up or down a level, and it is remembered:

| Command | What a tool call shows |
| --- | --- |
| `/collapse` | one line each, nothing under it |
| *(default)* | one line each, plus the diff when a file changed |
| `/expand` | arguments, the whole diff, the output, the model's reasoning as it streams, and what each round trip cost |

At full detail a turn narrates itself:

```
⏺ thinking
  │ the edit tool reported line two but the file says three, worth a look

  · 4.8k in · 121 out · 4.6k cached · 5.5s · 12% ctx
● 2 tool calls · 1 read · 1 edit
```

Terminal output only grows downwards, so calls already drawn keep the shape
they were drawn in. `/expand N`, `/expand last [k]` and `/expand all` reprint
calls from the journal at full detail, right where you are — including in a
resumed session. `/detail [collapsed|normal|expanded]` sets the level by name,
`ODEI_DETAIL` sets it for a run, and the statusline names it whenever it is
not the default.

A diff is computed by the tool that made the change, while both sides of the
file are still in hand, so it is the real change and not a guess: three lines
of context around each hunk, touching hunks merged, a single `⋯` for the gap
between them, line numbers from the file you now have, colour only on the
number and the sign. Long lines are cut rather than wrapped, a bounded body is
cut with a count, and a rewrite too large to align line by line says so
instead of pretending.

## Inspecting a tool call

Every completed activity line carries a handle:

```
● 3 tool calls · 2 reads · 1 command
  ├ Read src/agent.rs  142 lines  #1
  ├ Searched TODO  17 matches  #2
  └ Ran cargo test --lib ✗  exit 101 · test result: FAILED  #3
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

A call that changed a file gets a `── Diff` section ahead of its arguments —
for an edit, that *is* the arguments, read the way a person reads a change.

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
| `ODEI_DETAIL` | `collapsed` / `normal` / `expanded` — how much of each tool call to draw |
| `ODEI_MAX_AGENT_STEPS` | agent loop step cap (default 120) |
| `ODEI_THEME` | `light` / `dark` (auto-detected otherwise) |
| `ODEI_MARKDOWN` | `off` to print the model's markdown source verbatim |
| `ODEI_SPLASH` | `off` for the one-line greeting, `static` for the settled wordmark |
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
`mode`, `exit`. Out: `ready`, `state`, `history`, `waiting`, `thinking`, `step`, `text`,
`text_end`, `group`, `tool`, `approval`, `notice`, `turn_end`, `sessions`, `calls`,
`call`, `error`, `fatal`.

A `tool` event carries what the shell draws, already worked out: the tool name, the
label, the qualifier its arguments add, the one-glance `stat`, the elapsed `ms`, and —
for a call that changed a file — the `diff` as hunks of typed lines. A front end renders
a call without parsing tool output.

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
implementations, incl. `outline.rs`) · `ui.rs` (interactive shell) · `serve.rs` (NDJSON
front-end protocol) · `permissions.rs` · `session.rs` · `compact.rs` (history
summarization) · `calls.rs` (call journal + reports) · `inspect.rs` (picker + side pane) ·
`context.rs` (system prompt + runtime context) · `mentions.rs` (`@` attachments) ·
`commands.rs` (user-defined slash commands) · `complete.rs` (Tab completion) · `theme.rs`.

The Swift side splits into `OdeiCore` (protocol, process, transcript) and `OdeiUI`
(views), so the model layer can be checked without a window — XCTest ships with Xcode,
which this app deliberately does not require, so `OdeiChecks` is a plain executable that
exits non-zero on failure.

## Not implemented

MCP servers, skills, subagents, vision/image input, undo of file mutations, and an
LLM-based permission reviewer (a static classifier handles auto mode instead).

## License

Apache-2.0.
