# Agent Instructions

## Commands

The toolchain is pinned in `rust-toolchain.toml`. The suites also need
`missouri` and `gaff` on `PATH`:
`cargo install --locked --git https://github.com/cjohnhanson/missouri` and
`cargo install --locked --git https://github.com/cjohnhanson/gaff`.

| Task | Command |
|------|---------|
| Format and lint, as the commit gate does | `sh scripts/commit-gate.sh` |
| Every suite, as CI does | `sh scripts/test.sh` |
| One unit or integration test | `cargo test --all-features <name>` |
| Help snapshots after a help-text change | `KERSH_UPDATE_SNAPSHOTS=1 cargo test --all-features --test help` |
| The missouri suite | `cd tests/missouri && missouri run` |
| Install the commit and merge gates in a clone | `gaff init --git` |
| Regenerate the CI workflows after editing `.gaff/gaff.yml` | `gaff init --github` |

## Gates

| Gate | When | Runs |
|------|------|------|
| `commit-gate` | pre-commit | `scripts/commit-gate.sh`: rustfmt, clippy `-D warnings` |
| `merge-gate` | pre-push | `scripts/merge-gate.sh`: `scripts/test.sh` (cargo test, missouri, the seam against a real gaff) |
| `gate` | CI, every pull request | `scripts/merge-gate.sh` through `gaff ci`, then `gaff reviews check` on the head commit |
| `review-gate` | CI, every pull request | `.github/workflows/review-gate.yml`: main's policy judges the note on the head |
| `verify` | CI, every push | the commit gate and `scripts/test.sh` |

`gate.yml` and `verify.yml` are generated from `.gaff/gaff.yml`; `review-gate.yml` is hand-written and reads only main's files.

## Review

`.gaff/gaff.yml` requires one review, `review-code`. Tests and CI run before any reader.

1. Push the branch and open the pull request ready for review. `verify` and the test steps of `gate` are the CI-green signal; `gate` stays red until the note lands.
2. One reader, an agent that did not write the change, reads the CI-green head beside its run against `.agents/skills/review-code/SKILL.md`. It adds a line for each other vendored criterion the change touches: `review-tests`, `review-docs`, `review-deps`, `review-usability`. Add a second reader only for auth, secrets, network exposure, or data loss.
3. Fix every finding that breaks the goal, loses data, breaks execution, or leaves a gate red, with a test that failed first; decline the rest with a reason in the pull request. A new head needs a new round.
4. Write one note on the head (`git notes --ref=reviews add -m '<lines>' <sha>`, lines `signoff[<review>] PASS <sha> <evidence>`), push `refs/notes/reviews`, and rerun `gate` and `review-gate`.
5. Merge with a squash once every required check is green.

Leave `.agents/skills` as almanac vendors it; `almanac sync --check` reports drift.

## Layout

| Path | Contents |
|------|----------|
| `src/cli.rs` | `kersh::cli::main`, the whole command surface (clap) |
| `src/client.rs` | `kersh::Client`, the library API; every method has a command with the same JSON |
| `src/agent.rs`, `src/store.rs` | agent files and their roots |
| `src/model.rs`, `src/tools.rs`, `src/hook.rs`, `src/gaff.rs` | rig, the built-in harness, and its gaff governance |
| `tests/help.rs`, `tests/snapshots/help/` | `--help` snapshots for every command |
| `tests/missouri/` | state-transition suites; `gaff-runner` holds gaff's hook-agent contract |
| `tests/probes/` | probes of real harnesses; they need network and a signed-in harness, so no gate runs them |

## Principles

- **`kersh run` keeps gaff's contract.** gaff's hook agents run `kersh run <name> --context-file -` with a cleared environment and a stripped `PATH` and read a verdict from stdout; the `gaff-runner` suite holds it, so stdout stays the final text only.
- **kersh knows nothing about cards.** Labels are stored and echoed, never interpreted; no code reads or writes a card.
- **Every `Client` method has a command.** The command prints the same data as JSON lines with `--json`, so anything a linked program can do also works from a shell.
- **rmux stays behind the `Sessions` interface, pinned `=0.10.0`.** An upgrade is its own change with the gate rerun.
- **No credential code.** Credentials reach a harness only through `op run` and the harness's own login; kersh stores, mints, encrypts, and enforces none.
- **Tests use fixtures.** No test, probe, or capture reads a real card, note, queue, or transcript; probes read ids and event types only.

## External references

| Need | File |
|------|------|
| User docs | `README.md`, `docs/kersh.md` |
| Probe results the session code relies on | `tests/probes/RESULTS.md` |
