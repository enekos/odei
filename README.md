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

`read_file`, `write_file`, `edit_file`, `list_files`, `glob_files`, `grep_files`, `delete_file`,
`rename_file`, `copy_file`, `create_folder`, `file_info`, `semantic_search`, `terminal`
(foreground exec plus durable background sessions), `web_fetch`, `web_search`.

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
| `NO_COLOR` | disable styling |

Profile data lives in `~/.odei`: `config.json`, `permissions.json`, `sessions/*.jsonl`,
`usage.jsonl`, `history`.

## Develop

```bash
cargo build --release
cargo test
cargo clippy
```

Module map: `provider.rs` (Kimi SSE client) · `agent.rs` (tool loop) · `tools/` (registry +
implementations) · `ui.rs` (interactive shell) · `permissions.rs` · `session.rs` · `context.rs`
(system prompt + runtime context) · `theme.rs`.

## Not implemented

MCP servers, skills, subagents, vision/image input, undo of file mutations, and an
LLM-based permission reviewer (a static classifier handles auto mode instead).

## License

Apache-2.0.
