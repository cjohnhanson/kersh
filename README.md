# kersh

Run declarative agents. An agent file names a model, a skill and a gaff profile.

## Features

- Run agents through [rig](https://github.com/0xPlaygrounds/rig), kersh's built-in harness.
- Use the same file with a Claude subscription or API key.
- Append a skill's body to the system prompt.
- Render before calling a model.
- Limit model turns per run.
- Confine shell commands to a root.

## Installation

### From source

With Rust `1.88` or newer installed, clone the [repository](https://github.com/cjohnhanson/kersh) and enter its directory. You can then install kersh from that checkout and check its version:

```sh
cargo install --path .
kersh --version
```

[gaff](https://github.com/cjohnhanson/gaff) registers and runs guards. For `kersh doctor`, both kersh and gaff must be on your login `PATH`. Install gaff with:

```sh
cargo install --locked --git https://github.com/cjohnhanson/gaff
```

For the test suites, you also need `missouri` on `PATH`:

```sh
cargo install --locked --git https://github.com/cjohnhanson/missouri
```

## Usage

### Getting started

In your project directory, run:

```sh
kersh init
```

This creates `kersh.yml` and makes the directory a root. It also creates the reviewer agent file at `agents/reviewer/AGENT.md`. Agent files are Markdown with frontmatter:

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

After you edit the reviewer's job, check the files and render without calling the model. `--context-file -` reads context from standard input; the quoted argument supplies the prompt. Without `--context-file -`, render does not read the pipe:

```sh
kersh check
printf 'diff here\n' | kersh render reviewer --context-file - "hello"
```

`kersh check` exits `1` if a file fails to load. Rendering shows the system prompt and first user turn, with context before the prompt. To review a diff:

```sh
git diff main...HEAD | kersh run reviewer --context-file - "review this diff"
```

The final answer goes to standard output. Use `kersh list` to list agents and `kersh show reviewer` to inspect settings and the system prompt.

`--prompt-file` supplies the prompt from a file, or standard input with `-`. The current directory supplies the root and command directory unless you pass `--root`. `--output jsonl` emits a JSON event per line for turns, tool calls, and tool results, then the final text.

### Writing an agent file

Set `name` to the directory name and give `description` one line for `kersh list`. rig reads `model` as `<provider>/<model>`: `claude-code/<model>` runs the local `claude` CLI, while `anthropic/<model>` uses an API key. You may omit `model` for an agent that uses only another harness. Only `op run` and the harness's login supply its credentials.

Write the agent's identity in the body, which becomes its system prompt. Keep the situation out of this file; the caller and profile supply it at run time. A `profile` contains context, guards and a stop rule. If you declare one, gaff governs the run; without one, the run is ungoverned. The agent's only tool is a shell confined to the run's root, with commands governed by the profile.

almanac holds skills. When you set `skill`, kersh resolves it through almanac, appends the skill's body to the system prompt and aborts the run if the skill cannot be resolved. rig enforces `max_turns`, the most model turns a run may take, while `timeout` sets the deadline for each model call.

To choose how and where an agent runs, name a runtime in its `runtime` field. These presets live in `~/.config/kersh/runtimes.yml`. Settings apply in order: kersh's defaults, the runtime the agent names, the agent's own fields, `--runtime`, then individual flags. Later layers override earlier settings, but can only replace a `profile`, never remove it.

kersh finds `agents/<name>/AGENT.md` under the nearest root above your directory, with `~/.config/kersh/agents/` as the user-level fallback. The nearer root wins name collisions.

### Running sessions

A session runs a harness in a terminal pane. Pass an agent name to `kersh start`, or choose `--harness` or `--runtime` instead. Choose `rig`, `claude-code`, or `codex` for `--harness`:

```sh
kersh start --harness claude-code
kersh ps
```

`kersh ps` lists sessions. `kersh start` prints the session id for attaching:

```sh
kersh attach <id>
```

To detach without stopping, press `C-]`. rig defaults to `headless`; Claude Code and Codex default to `interactive`. `--mode` chooses either. A `headless` session requires `--prompt-file` and displays events without a controlling terminal; an `interactive` session accepts terminal input.

A `running` session has an active harness or awaits restore after a signal. A `held` session awaits a person or `kersh resume` after failure or interruption. Exit `0` or stopping means `ended`; disappearance without an exit note means `lost`.

`kersh resume` continues a `held` or `ended` session's conversation in a new pane. `kersh restart` restarts an `interactive` session on its conversation; `--mux` replaces the session server and restores sessions. With `kersh stop`, the session ends and its pane closes. An agent can stop itself:

```sh
kersh stop $KERSH_SESSION
```

`kersh send` types text and Enter into a running `interactive` session. `kersh events` prints headless events, `kersh screen` prints terminal contents, and `kersh rm` deletes `ended` sessions' records, events, and temp directories.

With `--group` and `--max-running`, starts at the group's limit exit `3`.

### Keeping sessions running

The host is the single writer of session records, one per user. Writes start it automatically by default; `kersh ps`, `kersh events`, and `kersh screen` work without it. `kersh host` runs it in the foreground.

The session server supplies terminals. The host captures a fresh login shell's variables as the login environment, then starts sessions with it. None come from the caller. A session is lost when its pane disappears while its record remains `running` or `lost`. The host restores sessions at startup and when its watcher detects a missing server or pane.

Install the `kersh-host` unit to start the host at login:

```sh
kersh service install
```

`kersh service stop` stops and removes the unit, leaving sessions running.

### Using other front ends

To browse and drive sessions in a terminal UI, run:

```sh
kersh tui
```

To expose commands through MCP on stdio, run:

```sh
kersh serve
```

These front ends and REST on `host.sock` use the same calls, with `GET /openapi.json` describing REST. Every `Client` method has a shell command; `--json` prints the same data, one JSON object per line. Commands send results to standard output and errors to standard error. Exit codes are `0` for success, `1` for failure, `2` for usage errors, and `3` at a group limit.

### Checking this machine

Check the installation against your login environment:

```sh
kersh doctor
```

`kersh doctor` reports harnesses, host, session server, the `kersh-host` unit, gaff and its hooks, guards for each harness and mode, and eng.rip tools without writing anything. It requires kersh and gaff on login `PATH`, gaff hooks for detected harnesses, and a running host or session server matching kersh's version, exiting `1` if a required piece is missing.

With `kersh setup claude-code` or `kersh setup codex`, you register gaff hooks and check wiring. Missing pieces produce exit `1`.

## Integration with gaff

Tool calls and stops go through the `gaff hook` executable, which kersh runs with `GAFF_HOST=generic`. The operator must permit the profile in `transitions.agent_may_set` before gaff accepts selection through `GAFF_PROFILE`. gaff refuses a tool call or stop with exit `2` and a reason on standard error.

Context injection uses standard output at a flush point. kersh flushes only at session start, so gaff cannot inject context mid-run.

For hook agents, gaff clears the environment, strips `PATH` and runs `kersh run <name> --context-file -`. The final answer on stdout gives gaff its verdict.

## Documentation

Print the full command and configuration manual:

```sh
kersh docs
```

You can also read the full manual on GitHub at [docs/kersh.md](https://github.com/cjohnhanson/kersh/blob/main/docs/kersh.md). For a short primer in an agent's context, use `kersh prime`.

## License

MIT.
