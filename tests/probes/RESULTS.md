# kersh step 1: harness probes

Run 2026-11-24 on macOS (Darwin 25.6.0). Claude Code 2.1.332 (`~/.local/bin/claude`), codex-cli 0.153.4, tmux (private server `tmux -L kprobe` as the pty for interactive runs), BSD `script -r` for the OSC recording. Ghostty 1.3.1 and WezTerm (nightly 2026-08-31) are installed.

Every harness ran from a scratch git repo (`<scratch>/proj`) under an environment with an enclosing Claude Code session's `CLAUDE*` variables removed (`env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE ...`), because an inherited `CLAUDE_CODE_CHILD_SESSION` turns transcripts off. Prompts were "reply with the word ok/two/three/four", "say hi", and "run `echo KERSH_PROBE_MARKER`". Only event types, field names, ids, exit codes, and yes/no facts were extracted (jq); the raw streams and recordings were deleted after extraction.

| # | Probe | Verdict |
|---|---|---|
| 1 | Ids in headless streams | Claude: session id in every event's `session_id`, equal to `--session-id`; `system/init` is **not** first when SessionStart hooks exist. Codex: `thread.started.thread_id` first. |
| 2 | Resume with a prompt | Claude interactive YES, Claude `-p` YES, `codex exec resume` YES; interactive `codex resume <id> <prompt>` NOT PROBED (trust dialog) |
| 3 | Codex instructions option | YES: `-c developer_instructions="..."` and `-c model_instructions_file="<path>"` both work |
| 4 | Fake guard | Claude `-p` REFUSED; Claude interactive REFUSED; `codex exec` REFUSED only for a trusted hook (untrusted hooks are skipped silently); interactive codex NOT PROBED |
| 5 | OSC 7501 | Claude sends only the query `OSC 7501 ; ? BEL` at start-up; with no reply it sends no OSC 7501 during a turn |
| 6 | Shift+Enter | Not run: kitty-protocol terminals are installed, but none can be scripted to send a real Shift+Enter key event without driving a desktop window |

## 1. Ids in headless streams

**Claude Code.** Command: `printf 'reply with the word ok' | claude -p --output-format stream-json --verbose --session-id <uuid>`, exit 0, empty stderr.

Event order seen (14 lines):

```
system/hook_started   x5   (hook_event = SessionStart)
system/hook_response  x5
system/init
assistant
rate_limit_event
result/success
```

- **`system/init` is not the first line.** With SessionStart hooks registered (the user's global gaff hooks here; kersh's own gaff registration will add the same), `system/hook_started` and `system/hook_response` events come first. kersh must find `init` by `type`+`subtype`, not by position.
- **Session id:** `session_id`, present on every event including the hook events, and equal to the `--session-id` uuid on all 14 lines.
- **`result`:** `type`="result", `subtype`="success", `is_error`=false, `total_cost_usd` (number), `session_id` = launch uuid.

Schema sketch (field names only):

- `system/hook_started`: `type, subtype, hook_event, hook_id, hook_name, session_id, uuid`
- `system/hook_response`: `type, subtype, hook_event, hook_id, hook_name, exit_code, outcome, output, stdout, stderr, session_id, uuid`
- `system/init`: `type, subtype, session_id, uuid, cwd, original_cwd, model, permissionMode, apiKeySource, tools, mcp_servers, agents, skills, slash_commands, terminal_slash_commands, plugins, output_style, claude_code_version, additional_directories, capabilities, sandbox_enabled, messaging_socket_path, fast_mode_state, fast_mode_disabled_reason, per_turn_effort_active, ultrathink_available, ultrathink_typed_raises, view_mode, analytics_disabled, product_feedback_disabled, served_by_anthropic`
- `assistant`: `type, message, parent_tool_use_id, request_id, session_id, timestamp, uuid` (+ `thinking_display, thinking_duration_ms` on thinking blocks, + `wire_tool_inputs` on tool use). `message`: `id, type, role, model, content, stop_reason, stop_sequence, stop_details, usage, container, context_management, diagnostics, input_transformations`. `content[]` types seen: `text`, `thinking`, `tool_use` (`type, id, name, input, caller`).
- `user` (tool results): `type, message, parent_tool_use_id, session_id, timestamp, tool_result_meta, tool_use_result, uuid`; `content[]` `tool_result`: `type, tool_use_id, content, is_error`.
- `rate_limit_event`: `type, rate_limit_info, session_id, uuid`
- `result`: `type, subtype, is_error, result, session_id, uuid, total_cost_usd, usage, modelUsage, model_usage_since_last_result, num_turns, duration_ms, duration_api_ms, ttft_ms, ttft_stream_ms, time_to_request_ms, first_content_frame_ms, stop_reason, terminal_reason, api_error_status, permission_denials, safety_stops, subagent_stats, queued_turn_count, result_index, credential_expires_at, fast_mode_state, fast_mode_disabled_reason`. `permission_denials[]`: `tool_name, tool_use_id, tool_input`.

**Codex.** Command: `printf 'reply with the word ok' | codex exec --json -`, exit 0. stderr had one `rmcp::transport::worker` ERROR log line (an MCP server from user config), so stderr is a log, not a protocol channel.

Event types seen across all runs: `thread.started`, `turn.started`, `item.started`, `item.completed`, `turn.completed`. `thread.started` was line 1 in every run.

- `thread.started`: `type, thread_id` (a UUIDv7, e.g. `01a210f6-...`)
- `turn.started`: `type`
- `item.started` / `item.completed`: `type, item`
  - `item.type` = `agent_message`: `id, type, text`
  - `item.type` = `command_execution`: `id, type, command, aggregated_output, exit_code, status` (`status` `in_progress` on start, `completed` on completion)
  - `item.type` = `error`: `id, type, message`
- `turn.completed`: `type, usage`; `usage`: `input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens`

No cost field in the Codex stream.

## 2. Resume with a prompt

Conversations from probe 1 were used.

| Command | Turn started with the prompt? | Evidence |
|---|---|---|
| `printf 'reply with the word two' \| claude -p --resume <uuid> --output-format stream-json --verbose` | YES | exit 0; `init` and `result` carry the same `session_id` as the original; assistant text "two"; `num_turns` 1 |
| `printf 'reply with the word two' \| codex exec --json resume <thread_id> -` | YES | exit 0; `thread.started.thread_id` equals the original id (no new thread); agent message "two" |
| `claude --resume <uuid> -- 'reply with the word three'` (interactive, tmux pane) | YES | after the trust dialog, the pane showed `⏺ three` with no typed input. A second run (`-- 'reply with the word four'`, folder already trusted) showed `⏺ four` and exited 0 on `/exit` |
| `codex resume <thread_id> 'reply with the word three'` (interactive) | NOT PROBED | the untrusted scratch folder shows Codex's trust dialog first; accepting it writes `[projects."<dir>"] trust_level` into `~/.codex/config.toml`, and the probes edit no user config. `-c 'projects."<dir>".trust_level="trusted"'` did not suppress the dialog (tested). The run was quit at the dialog; `~/.codex/config.toml` gained no entry for the scratch path |

`codex exec resume --help` documents the positional `[PROMPT]` ("If `-` is used, read from stdin"); `codex resume --help` documents `[PROMPT]` "Optional user prompt to start the session". Placing `--json` before `resume` (`codex exec --json resume <id> -`) works.

Interactive Claude trust dialog detail: when the folder has a `.claude/settings.json` with hooks, the dialog says "Trusting this folder loads .claude/settings.json (runs commands)" and the **default selection is "No, exit"**. `Enter` alone exits with status 1 ("Claude Code didn't start because this folder isn't trusted"); `Down` then `Enter` selects "Yes, I trust this folder".

## 3. Codex instructions option

`codex --help` and `codex exec --help` list no dedicated instructions flag; the route is `-c`. The config reference (learn.chatgpt.com/docs/config-file/config-reference) documents:

- `developer_instructions`: "Additional developer instructions injected into the session (optional)."
- `model_instructions_file`: "Replacement for built-in instructions instead of `AGENTS.md`."
- `instructions`: "Reserved for future use; prefer `model_instructions_file` or `AGENTS.md`."
- `experimental_instructions_file`: not on the page.

Proof, prompt "say hi" on stdin:

| Command | Reply |
|---|---|
| `codex exec --json -c 'developer_instructions="Always answer with the single word PINEAPPLE, whatever the user says."' -` | `PINEAPPLE` |
| `codex exec --json -c 'model_instructions_file="<path to file with that text>"' -` | `PINEAPPLE` |
| `codex exec --json -` (control) | `Hi!` |

Verdict: `developer_instructions` is the additive equivalent of Claude's `--append-system-prompt-file` (kersh reads the file and passes the text); `model_instructions_file` replaces Codex's built-in instructions and so is not the equivalent.

## 4. Fake-guard probe

Guard: one shell script, registered as a `PreToolUse` hook with matcher `Bash`; it exits 2 with `refused by kersh fake guard: probe marker` on stderr when `tool_input.command` contains `KERSH_PROBE_MARKER`, and logs one line (harness tag, event, tool name, decision) per call. Prompt: run `echo KERSH_PROBE_MARKER`.

| Harness and mode | Registration | Refused? | Evidence |
|---|---|---|---|
| Claude `-p` | project `proj/.claude/settings.json` | YES | `--allowedTools 'Bash(echo:*)'` so permissions would allow it; guard log `claude PreToolUse Bash REFUSED`; `tool_result.is_error` true with the guard's reason; `result.permission_denials` has 1 entry; exit 0 |
| Claude interactive | same file, folder trusted via the dialog | YES | guard log `claude PreToolUse Bash REFUSED`; no permission prompt appeared (the hook refused first); pane showed the refusal |
| `codex exec` | project `proj/.codex/hooks.json` | NO (hook never ran) | untrusted project: project layers are skipped. Also with `-c 'projects."<dir>".trust_level="trusted"'` and with `--dangerously-bypass-hook-trust`: guard log empty, command ran (`command_execution` exit 0). A project `.codex/config.toml` with a bogus `model` was also ignored under that `-c`, so the `-c` trust override does not load project layers |
| `codex exec` | CLI layer `-c 'hooks.PreToolUse=[{matcher="Bash",hooks=[{type="command",command="<guard>"}]}]'` | NO without hook trust; YES with `--dangerously-bypass-hook-trust` | without bypass: guard log empty, command ran. With bypass: guard log `codex-c PreToolUse Bash REFUSED`; no `command_execution` item at all; two `item.type=error` items ("`--dangerously-bypass-hook-trust` is enabled..."); stderr log line `codex_core::tools::router: error=Command blocked by PreToolUse hook: <reason>. Command: ...`; exit 0 |
| interactive codex | — | NOT PROBED | needs project trust persisted in `~/.codex/config.toml` (the trust dialog) or a user-level hook; the probes edit no user config |

Findings for guarded sessions:

- **Codex runs `PreToolUse` hooks under `codex exec`** (previously undocumented), and exit 2 + stderr blocks the command. The block shows in `--json` only as a missing `command_execution` item; the reason goes to stderr's log.
- **Codex skips an untrusted hook silently**: no event, no stderr line, and the command runs. Codex docs: "Non-managed hooks must be reviewed and trusted before they run"; project hooks load "only from trusted project layers". So rule 4's doctor check must test a hook *as registered by `gaff init`* (with its trust state), not just the presence of a hooks file; a registered-but-untrusted gaff hook leaves Codex unguarded with no visible sign.
- Codex docs list hook locations `/etc/codex/hooks.json`, `$CODEX_HOME/hooks.json`, trusted project `.codex/hooks.json`, and inline `[hooks]` in each layer's `config.toml`; hooks are on by default (`[features] hooks`, `codex features list`: `hooks stable true`).
- Claude interactive: a project `.claude/settings.json` with hooks makes the trust dialog default to "No, exit" (see section 2).

## 5. OSC 7501

Method: `script -q -r <rec> claude --resume <uuid> -- '<prompt>'` inside a `tmux -L kprobe` pane (120x36); a Python parser of the BSD record (`struct <QQII` len/sec/usec/direction) printed only OSC sequences. Two runs: one with a turn plus a tool-call turn (record ended when the pane was killed), and one with a turn and a clean `/exit` (status 0).

| Direction | Sequence | When |
|---|---|---|
| app → terminal | `ESC ] 7501 ; ? BEL` (payload `?`, a query) | once, 0.2–0.6 s after start |
| terminal → app | none (tmux does not answer OSC 7501) | — |
| app → terminal during the turn | **no OSC 7501** | — |

Other OSC seen: OSC 0 (window title) and OSC 8 (hyperlinks). Verdict: Claude Code only reports program status through OSC 7501 after a terminal answers its query, so the status payload shapes cannot be captured with tmux as the terminal; they need rmux 0.11 (which answers per its changelog) or another terminal that replies. Claude queries OSC 7501 at start-up and sends nothing more until answered.

## 6. Shift+Enter

Kitty-protocol terminals are installed: Ghostty 1.3.1 and WezTerm; kitty is not. Neither offers a CLI that sends a real key event: `wezterm cli send-text` sends text "as though it were pasted", and `ghostty +help` lists no input action. A real Shift+Enter needs key injection into a desktop window, and the check belongs to the rmux server with `extended-keys on`, which these probes do not run. Verdict: **not testable without driving a desktop window**; left for the session-host checks, through keystrokes into a dedicated WezTerm window.

## Cleanup

- Raw stream files (`*.jsonl`), stderr captures (`*.err`), and both `script -r` recordings were deleted after extraction, as was a temporary copy of `~/.codex/config.toml` used to check it was not modified.
- `tmux -L kprobe` server killed; `pgrep` for both conversation ids and the scratch path returns nothing.
- No user config was edited by these probes, except that accepting Claude's trust dialog for the scratch folder recorded that folder's trust in Claude's own state. `~/.codex/config.toml` has no entry for the scratch path.
- Kept in `probes/`: `fakeguard.sh`, `waitpane`, `osc7501.py`, `ce` (clean-env wrapper), the two driver scripts, `instr.md`, `fakeguard.log` (decision lines only), `claude-uuid`, `codex-tid`, and the scratch project with its hook files.
