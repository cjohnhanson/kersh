#!/bin/sh
# The merge gate. On pre-push it runs the suites. Under `gaff ci` on a
# pull request it runs the suites, then checks a sign-off for every
# review .gaff/gaff.yml declares on the pull request head.
#
# kersh merges through pull requests and main refuses direct pushes, so
# a branch push carries no review yet: the reader reads the CI-green
# head after the push. The review check belongs on the pull request.
#
# The review record is one git note on the head, one line per review:
#   git notes --ref=reviews add -m '<the lines>' <sha>
# Each line reads `signoff[<review>] PASS <sha> <evidence>`. Write a
# note only after an independent reader has read the change beside its
# CI run.
set -e

# git sends the ref list on stdin; drain it so no test reads it.
cat >/dev/null

sh scripts/test.sh

if [ "${GITHUB_ACTIONS:-}" != "true" ] || [ "${GITHUB_EVENT_NAME:-}" != "pull_request" ]; then
	echo "merge-gate: ok (the review check runs on the pull request)"
	exit 0
fi

# Every required review needs vendored criteria, so a name nobody can
# perform fails here rather than passing on an empty note.
required=$(gaff reviews)
set -f
for name in $required; do
	if [ ! -f ".agents/skills/$name/SKILL.md" ]; then
		echo "merge-gate: $name is required and has no criteria in .agents/skills." >&2
		exit 1
	fi
done
set +f

# A pull request event checks out a merge commit no reviewer saw. Check
# the branch head instead, read from the event payload.
command -v jq >/dev/null || {
	echo "merge-gate: jq is absent, so the pull request head sha cannot be read." >&2
	exit 1
}
head_sha=$(jq -r '.pull_request.head.sha // empty' "${GITHUB_EVENT_PATH:-/dev/null}")
case "$head_sha" in
[0-9a-f]*) ;;
*)
	echo "merge-gate: pull request head sha unreadable. Refusing." >&2
	exit 1
	;;
esac
echo "merge-gate: pull request. Reading the review note on $head_sha."
printf 'refs/heads/pr %s refs/heads/main 0000000000000000000000000000000000000000\n' "$head_sha" |
	gaff reviews check
echo "merge-gate: ok"
