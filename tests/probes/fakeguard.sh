#!/bin/sh
# Fake guard: refuse any shell command containing the probe marker.
in=$(cat)
cmd=$(printf '%s' "$in" | jq -r '.tool_input.command // (.tool_input.cmd|tostring) // ""' 2>/dev/null)
ev=$(printf '%s' "$in" | jq -r '.hook_event_name // "-"' 2>/dev/null)
tn=$(printf '%s' "$in" | jq -r '.tool_name // "-"' 2>/dev/null)
case "$cmd" in
  *KERSH_PROBE_MARKER*) echo "$(date +%s) $FG_TAG $ev $tn REFUSED" >> "$(dirname "$0")/fakeguard.log"; echo "refused by kersh fake guard: probe marker" >&2; exit 2;;
  *) echo "$(date +%s) $FG_TAG $ev $tn allowed" >> "$(dirname "$0")/fakeguard.log"; exit 0;;
esac
