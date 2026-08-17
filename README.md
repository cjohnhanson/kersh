# kersh

Run a declarative agent. An agent is one file. The file names a model, a
skill, and a gaff profile, and its body is the system prompt. kersh
resolves the references and runs the agent through
[rig](https://github.com/0xPlaygrounds/rig).

An agent runs on a Claude subscription through the local `claude` CLI, or
on an API key through rig's Anthropic provider, from the same file.

## Install

```sh
cargo install --path .
```

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
You review the provided diff for defects. Report defects only. Cite file
and line.
```

The body is the system prompt: the agent's identity, not its situation.
The situation comes from the caller and the profile at run time.

| Field | Meaning |
| --- | --- |
| `name` | The agent name. It must match the directory the file lives in. |
| `model` | `<provider>/<model>`. `claude-code/<model>` or `anthropic/<model>`. |
| `skill` | An almanac skill (recorded; the body is pasted into the system prompt for now). |
| `profile` | A gaff profile that carries the agent's context, guards, and stop rule. |
| `max_turns` | The most model turns a run may take. Default 4. |
| `timeout` | The per-model-call deadline. Default `120s`. |

## Stores

A directory with `kersh.yml` is a root. The nearest one up the tree is
the active root. `~/.config/kersh/agents/` is the user-level fallback. An
agent named `<name>` is `agents/<name>/AGENT.md` under a root. The nearer
root wins a name collision.

## Commands

```sh
kersh list                 # name every agent
kersh show <name>          # print an agent's frontmatter and body
kersh check                # validate every agent file
kersh render <name> [--context-file <path|->] [prompt]   # compose without a turn
kersh run <name> [--context-file <path|->] [--root <dir>] [prompt]
kersh docs
kersh prime
```

`render` prints the composed system prompt and first user turn and spends
nothing, so a prompt is debuggable before a run.

## The tools an agent gets

An agent gets three structured read tools, never a shell:

- `read_file(path)`: a file under the run's root, capped, with an escaping
  symlink refused.
- `grep(pattern, glob)`: ripgrep as a library, no shell and no honored
  ignore file.
- `list(glob)`: the files under the root that match a glob.

A shell command string cannot be guarded, so kersh does not give an agent
one. Write tools and a command runner are not in this release.

## Situation and safety

The caller supplies the situation with `--context-file`, or by a pipe:

```sh
git diff main...HEAD | kersh run reviewer --context-file - "review this diff"
```

kersh wraps the context in a per-run marker, because a diff or an issue
body is untrusted text and must not gain instruction authority over the
model. The model string is validated before it could reach a child
process's argument vector, because a value such as `haiku --settings=...`
would otherwise execute a hook before any turn.

## Status

This is v0.1.0: agent files, the read tools, prompt composition, both
providers, and the command surface. gaff governance (a profile's guards,
context, and stop rule enforced at run time) is the next milestone; it
waits on a gaff change that lets a second host receive gaff's output.

## License

MIT.
