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

`runtime` names the default preset for how and where execution happens. A gaff `profile` carries context, guards and a stop rule. gaff registers and runs guards. If you specify `skill`, kersh appends that almanac skill's body to the system prompt using `KERSH_ALMANAC` (default `almanac`) to resolve it and aborting execution if resolution fails.

rig enforces the `max_turns` limit on model turns while `timeout` sets each model call's deadline using bare seconds or a duration such as `120s` or `2m`. The body defines identity through the system prompt; the caller and profile supply the situation at run time.

A root contains `kersh.yml` and the nearest root up the tree is active. Nearer roots win name collisions with `~/.config/kersh/agents/` as the fallback. For `list`, `show`, `check`, `render` and `run`, `--root` selects a root; for `init` it selects the creation directory.

`kersh list` shows agents and descriptions while `kersh show reviewer` prints settings and the system prompt. `kersh check` validates every agent and exits `1` on failure. You can copy into `~/.config/kersh/agents/<name>/AGENT.md` with `kersh agent add <FILE>` and replace differing contents by adding `--force`.

## Prompt input and execution

You can pass a prompt as an argument and supply context through a pipe:

```sh
git diff main...HEAD | kersh run reviewer --context-file - "review this diff"
```

A pipe alone supplies no input: use `--context-file -` for context or `--prompt-file -` for the prompt, or give either option a filename. If your prompt begins with a dash, put `--` before it. `kersh render reviewer "hello"` prints the system prompt and first user turn without calling the model.

kersh wraps context in a per-run marker to deny it instruction authority. Before constructing child-process arguments, it validates model strings so embedded options cannot execute hooks.

`kersh run` uses rig with the provider selected by the `model` prefix: `claude-code/<model>` uses your local `claude` CLI and Claude subscription while `anthropic/<model>` uses rig's Anthropic provider with an API key.

For `run`, `--root` selects the root and working directory (default: your current directory). The agent has only the `bash` tool with a `30`-second deadline and `64 KiB` output cap, confined to that root under the profile's command rules. If the runtime selects another harness, `run` exits `1` and directs you to `kersh start`.

## Sessions

The host is the sole writer of session records for each user and serializes changes and exit recording under its lock. A session runs a harness in a terminal pane managed by the session server in a separate rmux process that keeps agents running across host restarts.

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

You can continue a `held` or `ended` conversation in a new pane with `resume`, supplying `--prompt-file` with a path or `-` for stdin when the session is headless. `restart` restarts an interactive session on its conversation:

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

With `local` as the only available `environment`, runtimes accept `harness`, `mode` and `model` alongside `profile` for governance and `resume_prompt` for interactive resume where the harness accepts it. `tempdir` supplies a temporary directory retained with the session record while literal session variables go in `env` and harness options go in `flags`.

To prevent changes to harness configuration, hooks or code loading, runtime `env` rejects these prefixes and names:

- Prefixes: `CLAUDE_`, `CODEX_`, `ANTHROPIC_`, `OPENAI_`, `GAFF_`,
  `KERSH_`, `XDG_`, `LD_`, `DYLD_`, `BUN_`, `NODE_`.
- Names: `HOME`, `PATH`, `SHELL`, `COLORTERM`, `COLORFGBG`.

The prefix rule also covers future variables in those families. Although these restrictions guard against misconfiguration, they do not constrain other programs an agent can start as your user.

Harness flags after `--` and runtime `flags` must pass a per-harness allow-list that denies unlisted options. It excludes options that skip or replace gaff hooks: Claude Code's `--bare`, `--setting-sources` and `--settings` (kersh composes settings) plus Codex's `--dangerously-bypass-hook-trust`, `--enable`, `--disable` and `-c` keys under `features` or `hooks`.

Credentials reach a harness only through `op run` or the harness's own login. kersh does not store, mint, encrypt or enforce them.

## Configuration

Configuration lives under `~/.config/kersh/` regardless of your `XDG_CONFIG_HOME` setting, with host settings in `config.yml` and presets in `runtimes.yml` beside the user-level `agents/` directory.

In `config.yml`, `memory.root` selects the default notes store for `kersh memories` and supplies `KERSH_MEMORY_ROOT` to every session. When you start a session outside a terminal, `pane` sets its size. `colorfgbg` supplies every pane's `COLORFGBG` with a default of `15;0` (light on dark).

`restore` controls whether the host brings lost sessions back and defaults to `true`. To set the interval between session screen captures, use `capture_seconds` (default `180` seconds).

State lives in `$XDG_STATE_HOME/kersh/` with `~/.local/state/kersh/` as the default. It contains the host and session server locks and sockets, `groups.json` and one directory per session. Within the state directory permissions are `0700` for the directory and `0600` for its files.

## Host service

`kersh service install` writes and loads the `kersh-host` unit in `~/Library/LaunchAgents` (launchd) or `~/.config/systemd/user` (systemd) to run now and at login with restore. Identical reinstallation changes nothing; `kersh service stop` stops and removes it while sessions continue:

```sh
kersh service install
kersh service stop
```

In `config.yml`, `service_manager` defaults to macOS `launchd`, Linux `systemd` with a user manager, otherwise `none`. With default `auto_start: true`, writes start an absent host through its installed unit or a one-shot launchd job, transient systemd unit or detached process if none is installed. These survive terminal closure.

## Governance with gaff

When you declare a `profile`, kersh runs under gaff; without one it runs ungoverned. Before a profiled start, kersh checks that gaff knows the profile, that the harness and mode run its hooks and that registration matches the harness's settings location. Missing or broken gaff aborts execution. `KERSH_GAFF` selects the binary (default `gaff` on `PATH`).

Register hooks for your harness with `setup`:

```sh
kersh setup claude-code
kersh setup codex
```

`setup` runs `gaff init --host <harness>` and checks the wiring before exiting `1` if a piece is missing. gaff writes registration without kersh writing any harness settings or hook files.

For rig kersh calls `gaff hook` with `GAFF_HOST=generic` and selects `profile` through `GAFF_PROFILE`. You must permit the profile under `transitions.agent_may_set` in `gaff.yml`. Without configuration gaff itself does nothing. gaff refuses a tool call or stop with exit `2` and a reason on stderr. At a flush point it injects context on stdout.

gaff base guards for Claude Code's `Bash` tool and `command` field also guard kersh's `bash`, which sends the same payload shape. Guards for other tool names do not apply. kersh flushes only at session start, so gaff cannot inject context mid-run.

gaff hook agents invoke `kersh run <name> --context-file -` with a cleared environment and stripped `PATH`, then read final verdict text from stdout.

## Agent memory

`kersh memories` produces a gaff-injected block capped at `4,000` bytes by retrieving notes tagged `agent-memory` or `agent-memory-processed` through `zettel note list --format json` without opening note files. If you need another store, `--root` overrides the configured `memory.root`:

```sh
kersh memories --root <DIR>
```

## Output and exit codes

Every operation available to a program linking kersh has a matching shell command. With `--json`, commands print the same data those calls return, one JSON object per line.

The `check --json` summary counts agents and problems, with `roots` ordered from the nearest directory outward:

```sh
$ kersh check --json
```

`run --output text` (default) prints only the final answer while `jsonl` prints one JSON line per turn, tool call, tool result and assistant text before the result with final text. Results use stdout and errors use stderr.

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Failure |
| `2` | Usage error |
| `3` | Limit reached |

kersh command exit codes.

Unknown subcommands return `2` while starts at a group's `--max-running` limit return `3`.

## Machine checks

Check your machine with `doctor`, or list harness paths and versions with `harness list`:

```sh
kersh doctor
kersh harness list
```

Against the login environment, `doctor` checks harnesses and their guards in each mode, the host and session server, the `kersh-host` unit, gaff hooks and eng.rip tools. It requires `kersh` and `gaff` on login `PATH`, hooks for detected harnesses and a running host or session server matching this kersh version.

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
