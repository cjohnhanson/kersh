# Run agents with kersh

kersh runs declarative agents. Use `kersh docs` for this manual and `kersh prime` for an agent primer. You can check your version with `kersh --version`.

## Agent files

An agent file is Markdown with YAML frontmatter at `agents/<name>/AGENT.md`. When you run `kersh init [NAME]`, it creates `kersh.yml` and a starter agent while keeping existing files. The default name is `reviewer`:

```sh
kersh init
kersh check
```

Your starter file contains:

```markdown
---
name: reviewer
description: One line, shown by `kersh list`.
model: claude-code/haiku
max_turns: 4
timeout: 120s
---
State the agent's job here: what it does, what it reports, and
what it must not do. This body is the system prompt.

The caller supplies the situation with `--context-file` or a pipe.
Use the bash tool to read files: cat, ls, rg, git diff, and so on.
```

`name` must match the directory while `description` appears in `kersh list`. A harness runs your agent: built-in `rig`, Claude Code (`claude-code`) or Codex (`codex`). For rig the `model` field takes `<provider>/<model>`, which agents running only on another harness can omit.

`runtime` names the default execution preset. A gaff `profile` carries context, guards and a stop rule. gaff registers and runs guards. If you specify `skill`, kersh appends the almanac skill's body to the system prompt. It resolves skills through `KERSH_ALMANAC` (default `almanac`) and aborts if resolution fails.

rig limits model turns to `max_turns`. Each model call has a `timeout` deadline, expressed as bare seconds or a duration such as `120s` or `2m`. The body defines identity through the system prompt; the caller and profile supply the situation at run time.

A root contains `kersh.yml` and the nearest root up the tree is active. Nearer roots win name collisions with `~/.config/kersh/agents/` as the fallback. For `list`, `show`, `check`, `render` and `run`, `--root` selects a root; for `init` it selects the creation directory.

`kersh list` shows agents and descriptions while `kersh show reviewer` prints settings and the system prompt. `kersh check` validates every agent and exits `1` on failure. You can copy into `~/.config/kersh/agents/<name>/AGENT.md` with `kersh agent add <FILE>` and replace differing contents by adding `--force`.

## Prompt input and execution

You can pass a prompt as an argument and supply context through a pipe:

```sh
git diff main...HEAD | kersh run reviewer --context-file - "review this diff"
```

A pipe alone supplies no input. Use `--context-file -` for context or `--prompt-file -` for the prompt; either option also accepts a filename. If your prompt begins with a dash, put `--` before it. `kersh render reviewer "hello"` prints the system prompt and first user turn without calling the model.

kersh wraps context in a per-run marker to deny it instruction authority. Before constructing child-process arguments, it validates model strings so embedded options cannot execute hooks.

`kersh run` uses rig. The `model` prefix selects the provider: `claude-code/<model>` uses the local `claude` CLI and your subscription; `anthropic/<model>` uses an API key.

`run --root` selects the working directory, defaulting to the current directory. Its `bash` tool executes `sh -c` there, with a `30`-second deadline and `64 KiB` output cap. A `profile` routes calls through `gaff hook`; without one, commands are unrestricted. If the runtime selects another harness, `run` exits `1` and directs you to `kersh start`.

## Sessions

The host is each user's sole writer of session records, serializing changes and exit recording under its lock. A session runs a harness in a terminal pane. The session server manages panes in a separate rmux process, keeping agents running across host restarts.

`kersh host` runs in the foreground and starts a missing session server. At startup the host captures a fresh login shell's variables once as the login environment, which sessions inherit instead of the invoking process's variables. `kersh ps`, `events` and `screen` read files and the session server without the host.

### Harness and mode selection

`kersh start` prints a session id and requires an agent, `--harness` or `--runtime`. Harness flags follow `--` and must pass the harness's allow-list:

```sh
kersh start reviewer --prompt-file task.txt
kersh start --harness codex
kersh start --runtime worker --prompt-file task.txt
```

With `--mode` you select `headless` or `interactive`. The default harness is `rig` with mode `headless` while Claude Code and Codex default to `interactive`. Headless execution needs `--prompt-file` for stdin and has no controlling terminal, so opening `/dev/tty` fails instead of blocking. The wrapper reads piped stdout and stderr and writes one rendered line per event to the pane.

Settings resolve through kersh defaults, the agent's runtime, its fields, your `--runtime` preset and then individual flags. Although later settings win, a profile can only be replaced by another, never removed.

A group collects sessions for `--max-running`, with `--group` selecting the group whose limit refuses starts with exit `3`. `--label KEY=VALUE` values are stored and echoed without interpretation. For remaining options see `kersh start --help`.

The `KERSH_SESSION` variable contains the session id so an agent can stop itself:

```sh
kersh stop "$KERSH_SESSION"
```

### Controlling a session

`running` means executing or awaiting restore after `SIGTERM`, `SIGHUP` or terminal hang-up. Exit `0` or `stop` sets `ended` and closes either mode's pane. After other exits or headless interrupts, the pane stays open with state `held` for manual action or `resume`. Disappearance without an exit note means `lost`.

With `ps --state held` you filter records; `--follow` prints records then changes until interrupted. Empty results exit `0`:

```sh
$ kersh ps
no sessions. `kersh start` starts one.
```

`attach` opens the terminal while `C-]` leaves it running. For running interactive sessions, `send` types text then Enter; `--prompt-file` accepts a file or `-` (default stdin):

```sh
kersh attach <id>
kersh send <id> --prompt-file task.txt
```

`events` prints headless events as JSON lines after sequence `--since` (default `0`); `--text` selects summaries and `--follow` continues until execution stops. `screen` prints plain text, with `--follow` repeating it on changes:

```sh
kersh events <id> --since 0 --follow
kersh screen <id> --follow
```

`rm` deletes an ended session's record, events and temporary directory.

### Restoring sessions

Use `resume` to continue a `held` or `ended` conversation in a new pane. Headless sessions require `--prompt-file` with a path or `-` for stdin. For an interactive session, `restart` restarts it on its conversation:

```sh
kersh resume <id> --prompt-file task.txt
kersh restart <id>
```

To replace your session server and restore every session after a wire-version change, use:

```sh
kersh restart --mux
```

The host restores missing panes whose records say `running` or `lost` at startup, after server loss or when a pane disappears. If you set `restore: false`, it disables restoration.

When their saved conversation exists, interactive Claude Code and Codex sessions resume with `resume_prompt`; other sessions return with records `held` and their command ready. Panes retain size, directory and environment with the last screen capture and restore line printed above the agent. If another record or outside process holds the conversation, restoration leaves the record `held` to prevent concurrent processes.

## Runtimes

Runtimes live in `~/.config/kersh/runtimes.yml`. To create your `worker` preset, put these fields in `worker.yml`:

```yaml
harness: claude-code
mode: headless
model: opus
```

You can add or replace the preset and then inspect it with these commands:

```sh
kersh runtime add worker worker.yml
kersh runtime list
kersh runtime show worker
```

Runtime fields `harness`, `mode` and `model` select execution settings; `environment` accepts only `local`. The `profile` field selects governance, and `resume_prompt` supplies interactive resume input where the harness accepts it. `tempdir` keeps a temporary directory with the session record. Use `env` for literal session variables and `flags` for harness options.

To prevent changes to harness configuration, hooks or code loading, runtime `env` rejects these prefixes and names:

- Prefixes: `CLAUDE_`, `CODEX_`, `ANTHROPIC_`, `OPENAI_`, `GAFF_`,
  `KERSH_`, `XDG_`, `LD_`, `DYLD_`, `BUN_`, `NODE_`.
- Names: `HOME`, `PATH`, `SHELL`, `COLORTERM`, `COLORFGBG`.

The prefix rule also covers future variables in those families. Although these restrictions guard against misconfiguration, they do not constrain other programs an agent can start as your user.

Harness flags after `--` and runtime `flags` must pass a per-harness allow-list that denies unlisted options. Options that skip or replace gaff hooks are excluded. For Claude Code these include `--bare`, `--setting-sources` and `--settings` (kersh composes settings). For Codex they include `--dangerously-bypass-hook-trust`, `--enable`, `--disable` and `-c` keys under `features` or `hooks`.

rig's Anthropic client gets its API key through environment variables. Harnesses inherit the login environment; kersh provides no credential storage, creation, encryption or enforcement.

## Configuration

Configuration lives under `~/.config/kersh/` regardless of your `XDG_CONFIG_HOME` setting, with host settings in `config.yml` and presets in `runtimes.yml` beside the user-level `agents/` directory.

In `config.yml`, `memory.root` selects the default notes store for `kersh memories` and supplies `KERSH_MEMORY_ROOT` to every session. When you start a session outside a terminal, `pane` sets its size. `colorfgbg` supplies every pane's `COLORFGBG` with a default of `15;0` (light on dark).

`restore` controls whether the host brings lost sessions back and defaults to `true`. To set the interval between session screen captures, use `capture_seconds` (default `180` seconds).

State lives in `$XDG_STATE_HOME/kersh/` with `~/.local/state/kersh/` as the default. It contains the host and session server locks and sockets, `groups.json` and one directory per session. Within the state directory permissions are `0700` for the directory and `0600` for its files.

## Host service

`kersh service install` writes then loads the `kersh-host` unit in `~/Library/LaunchAgents` (launchd) or `~/.config/systemd/user` (systemd), starting now and at login with restore. Identical reinstalls change nothing. `kersh service stop` removes the unit; sessions continue:

```sh
kersh service install
kersh service stop
```

`config.yml` defaults: `service_manager` is macOS `launchd`, Linux `systemd` with a user manager, otherwise `none`; `auto_start` is `true`. With auto-start enabled, `start`, `stop`, `resume`, `restart`, `rm`, `send` and `ps --follow` start a missing host through its installed unit. Without a unit, the client starts a one-shot launchd job, transient systemd unit or detached process, according to `service_manager`; the host survives terminal closure. With `none`, installation fails with exit `1`; select `launchd` or `systemd`.

## Governance with gaff

With a `profile`, kersh runs under gaff; without one it runs ungoverned. Before starting, kersh checks that gaff knows the profile and the harness and mode run its hooks. It also checks registration at the harness's settings location. Missing or broken gaff aborts execution. `KERSH_GAFF` selects the binary (default `gaff` on `PATH`).

Register hooks for your harness with `setup`:

```sh
kersh setup claude-code
kersh setup codex
```

`setup` runs `gaff init --host <harness>` and checks the wiring before exiting `1` if a piece is missing. gaff writes registration without kersh writing any harness settings or hook files.

For rig kersh calls `gaff hook` with `GAFF_HOST=generic` and selects `profile` through `GAFF_PROFILE`. Permit the profile under `transitions.agent_may_set` in `gaff.yml`. Without configuration gaff itself does nothing. gaff refuses a tool call or stop with exit `2` and a reason on stderr. At kersh's session-start flush point, it injects context on stdout.

gaff base guards for Claude Code's `Bash` tool and `command` field also guard kersh's `bash`, which sends the same payload shape. Guards for other tool names do not apply. kersh flushes only at session start, so gaff cannot inject context mid-run.

gaff hook agents invoke `kersh run <name> --context-file -` with a cleared environment and stripped `PATH`, then read final verdict text from stdout.

## Agent memory

`kersh memories` retrieves notes tagged `agent-memory` or `agent-memory-processed` through `zettel note list --format json`. It produces a gaff-injected block capped at `4,000` bytes without opening note files. If you need another store, `--root` overrides the configured `memory.root`:

```sh
kersh memories --root <DIR>
```

## Output and exit codes

Every operation available to a program linking kersh has a matching shell command. With `--json`, commands print the same data those calls return, one JSON object per line.

The `check --json` summary counts agents and problems, with `roots` ordered from the nearest directory outward:

```sh
$ kersh check --json
{"kind":"summary","agents":1,"problems":0,"ok":true,"roots":[...]}
```

`run --output text` defaults to final-answer output. With `jsonl`, each turn, tool call, tool result and assistant text gets a JSON line; the final result comes last. Results use stdout; errors use stderr.

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Other failure |
| `2` | CLI parsing or session usage error |
| `3` | Group limit reached |

kersh command exit codes.

Unknown subcommands and session usage errors return `2`; missing input to `kersh run <agent>` returns `1`. Group limits return `3`.

## Machine checks

Check your machine with `doctor`, or list harness paths and versions with `harness list`:

```sh
kersh doctor
kersh harness list
```

`doctor` checks harnesses, guards, host, session server, `kersh-host` unit and gaff hooks against the login environment. It requires `kersh` and `gaff` on login `PATH` and hooks for detected harnesses. Running hosts and session servers must match this kersh's version. Optional eng.rip checks cover zettel memory, almanac skills, tisket tasks and plans, tundish delivery, and their configuration.

If pieces marked `required` are missing, it exits `1`. Checks read files and run `--version` without writes; `--json` identifies each check:

```sh
$ kersh doctor --json
{"area":"harness","check":"rig","state":"ok","required":false,"detail":"built in"}
```

## Front ends

MCP and REST share operations and command `--json` result shapes with the terminal UI. Nothing listens on TCP.

### MCP

Start the stdio server with:

```sh
kersh serve
```

Your MCP application owns stdin and stdout while tools return JSON lines. Because `attach` requires a terminal, it has no tool.

Session tools are `kersh_ps`, `kersh_start`, `kersh_stop`, `kersh_resume`, `kersh_restart`, `kersh_send`, `kersh_events` and `kersh_screen`. For agents use `kersh_agent_list` and `kersh_agent_show`; for runtimes use `kersh_runtime_list` and `kersh_runtime_show`. Harness inspection and hook registration use `kersh_harness_list` and `kersh_setup`.

### REST

The state directory's `host.sock` serves `HTTP/1.1` with JSON bodies matching command `--json` output, arrays for lists and `code` in errors, as `/openapi.json` describes. With access restricted to your processes by the `0700` directory, there is no application authentication.

Paths include `/v1/sessions` and `/v1/sessions/{id}` with `/stop`, `/resume`, `/restart`, `/send`, `/events` and `/screen` suffixes, plus `/v1/changes`, `/v1/harnesses`, `/v1/runtimes` and `/v1/version`.

Requests and responses carry `kersh-schema` and `kersh-version` with both sides refusing schema mismatches and naming both versions. Without these headers requests use this build's schema. You can access the socket from a shell through `curl --unix-socket`.

### Terminal UI

Browse sessions with:

```sh
kersh tui
```

When you attach, the UI releases the terminal to foreground `kersh attach <id>`; `C-]` ends that child and returns to the list.

| Keys | Action |
| --- | --- |
| `j`, `<down>` / `k`, `<up>` | Next/previous |
| `<home>` / `<end>` | First/last |
| `s` / `x` | Start/stop |
| `r` / `R` | Resume/restart |
| `a`, `RET` | Attach |
| `i` | Send text |
| `d` | Remove ended session |
| `v` | Watch screen here |
| `g` | Refresh |
| `?` / `q` | Help/quit |

Default session-list keys in `kersh tui`.

Set keybindings in `~/.config/kersh/keys.yml`; help follows those bindings. The Cargo build flag `--no-default-features` omits the default `tui` feature.
