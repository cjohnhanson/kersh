# kersh

Run a declarative agent. An agent is one file. The file names a model, a
skill, and a gaff profile, and its body is the system prompt. kersh
resolves the references and runs the agent through rig.

## The agent file

`agents/<name>/AGENT.md`, markdown with frontmatter:

```markdown
---
name: reviewer
description: Reviews a diff for defects.
model: claude-code/haiku
skill: code-review
profile: reviewer
max_turns: 4
timeout: 120s
---
You review the provided diff for defects. Report defects only.
```

The body is the system prompt: identity only. The gaff profile named by
`profile` carries the agent's context, its guards, and its stop rule. The
`skill` names an almanac skill; kersh appends its body to the system
prompt as the agent's method, and aborts if it cannot resolve.

## Stores

A directory with `kersh.yml` is a root. The nearest one up the tree is
the active root. `~/.config/kersh/agents/` is the user-level fallback.
An agent named `<name>` is `agents/<name>/AGENT.md` under a root. The
nearer root wins a name collision.

## Commands

- `kersh list` names the agents.
- `kersh show <name>` prints an agent's frontmatter and body.
- `kersh check` validates every agent file.
- `kersh render <name> [--context-file <path|->] [prompt]` prints the
  composed system prompt and first user turn without spending a turn.
- `kersh docs` prints this document.
- `kersh prime` prints a short primer for an agent's context.

## The model

The `model` field is `<provider>/<model>`. `claude-code/<model>` runs the
local `claude` CLI so a turn draws on a Claude subscription.
`anthropic/<model>` uses an API key. The value is validated before it
reaches a child process, because a model string is a command argument.

## Governance

An agent that declares a `profile` runs under gaff. kersh calls `gaff
hook` at each tool call and each stop, so the profile's guards and stop
rule apply, and it prepends the profile's session-start context to the
first turn. Governance is opt-in: without a `profile`, kersh runs
ungoverned. With one, a broken or absent gaff aborts the run rather than
degrading. `KERSH_GAFF` names the gaff binary; the default is `gaff` on
`PATH`. A gaff guard for a kersh agent names kersh's own tools
(`read_file`, `grep`, `list`).
