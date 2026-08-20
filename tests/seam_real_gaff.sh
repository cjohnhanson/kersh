#!/bin/sh
# The seam against the real gaff binary.
#
# The hermetic missouri suites drive a fake gaff authored in kersh's own
# payload vocabulary, so they prove kersh's hook logic, not that kersh
# talks to the real gaff. This integration check closes that gap: it runs
# kersh against a real, merged gaff with a real profile guard, so a drift
# between kersh's emitted vocabulary and gaff's generic host breaks a gate.
#
# It is not hermetic: it needs a merged gaff with the generic host and
# profile bundles (the hrt5 and 720x work). CI builds that gaff. Run it
# locally with a merged gaff:
#   tests/seam_real_gaff.sh /path/to/merged/gaff /path/to/kersh
#
# Args: $1 gaff binary (default: gaff on PATH). $2 kersh binary (default:
# kersh on PATH). kersh must be built with the `fake-model` feature.
set -eu

GAFF="${1:-gaff}"
KERSH="${2:-kersh}"

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
export HOME="$T/home" GAFF_STATE_DIR="$T/state" KERSH_GAFF="$GAFF"
mkdir -p "$HOME/.config/gaff" "$T/root/agents/reviewer" "$T/root/notes"
: >"$T/root/kersh.yml"

cat >"$T/root/agents/reviewer/AGENT.md" <<'MD'
---
name: reviewer
model: fake/scripted
profile: reviewer
max_turns: 4
---
You review the notes.
MD

# A profile bundle whose guard names kersh's own tool and field. gaff
# honors GAFF_PROFILE=reviewer because agent_may_set lists it.
cat >"$HOME/.config/gaff/gaff.yml" <<'EOF'
profiles:
  reviewer:
    base: false
    guards:
      - name: no-secrets
        tool: bash
        field: command
        matches: 'secret'
        message: That path holds secrets.
transitions:
  agent_may_set:
    - reviewer
EOF

# A stale gaff (before the generic host and profile bundles) cannot run
# this. Say so plainly rather than failing on a downstream assertion.
if ! "$GAFF" check >/dev/null 2>&1; then
	echo "FAIL: gaff rejects the profile config; you need a merged gaff (hrt5 + 720x)."
	"$GAFF" check || true
	exit 1
fi

printf 'TOPSECRET-42\n' >"$T/root/notes/secret.txt"
printf 'hello world\n' >"$T/root/notes/public.txt"

# Refuse: a guarded path never reaches the model.
export KERSH_FAKE_SCRIPT='[{"kind":"tool","name":"bash","args":{"command":"cat notes/secret.txt"}},{"kind":"echo_tool_result"}]'
OUT=$("$KERSH" run reviewer --root "$T/root" "review" 2>&1) || {
	echo "FAIL: a guarded run must not error: $OUT"
	exit 1
}
case "$OUT" in
*TOPSECRET-42*)
	echo "FAIL: the real gaff guard did not block the read: $OUT"
	exit 1
	;;
esac
case "$OUT" in
*"no-secrets"* | *"secrets"*) ;;
*)
	echo "FAIL: the model must see the real guard's refusal: $OUT"
	exit 1
	;;
esac

# Allow: an unguarded path runs and its content reaches the model.
export KERSH_FAKE_SCRIPT='[{"kind":"tool","name":"bash","args":{"command":"cat notes/public.txt"}},{"kind":"echo_tool_result"}]'
OUT=$("$KERSH" run reviewer --root "$T/root" "review" 2>&1) || {
	echo "FAIL: an allowed run must not error: $OUT"
	exit 1
}
case "$OUT" in
*"hello world"*) ;;
*)
	echo "FAIL: an unguarded read must reach the model: $OUT"
	exit 1
	;;
esac

echo SEAM_OK
