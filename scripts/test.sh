#!/bin/sh
# The suites: cargo test with every feature, the missouri suite, and the
# seam against a real gaff. merge-gate.sh runs this before the review
# check; CI's verify job runs it on every push.
set -e

# --all-features, because the fake-model feature is shipped test
# support and the suites drive it.
echo "test: cargo test"
test_out=$(cargo test --workspace --all-features --quiet 2>&1 </dev/null) || {
	echo "test: cargo test failed. Nothing merges on red tests." >&2
	printf '%s\n' "$test_out" | tail -40 >&2
	exit 1
}

# A CI runner has no nix; missouri installs the packages the suites
# declare there. A local run keeps the nix backend.
if [ -n "${CI:-}" ]; then
	MISSOURI_SANDBOX=preinstalled
	export MISSOURI_SANDBOX
fi

command -v missouri >/dev/null || {
	echo "test: missouri is not on PATH." >&2
	echo "  cargo install --locked --git https://github.com/cjohnhanson/missouri" >&2
	exit 1
}
# The suites run `kersh` from PATH. Under the preinstalled sandbox
# missouri skips the suite's own build step, so build the fake-model
# binary here and put it first on PATH in both modes.
cargo build --quiet --features fake-model
PATH="$(pwd)/target/debug:$PATH"
export PATH
echo "test: missouri run"
out=$(cd tests/missouri && missouri run </dev/null 2>&1) || {
	echo "test: the missouri suite failed." >&2
	printf '%s\n' "$out" | tail -20 >&2
	exit 1
}
# A discovery or path error could exit 0 having run none of the governed
# suites. Every named suite must report its own pass line. Strip colour
# first, because missouri colours the PASS token.
plain=$(printf '%s\n' "$out" | sed 's/\x1b\[[0-9;]*m//g')
for suite in init governed-allow governed-refuse governed-streak \
	governed-hold governed-context governed-fail \
	skill-applied skill-missing gaff-runner; do
	printf '%s\n' "$plain" | grep -q "PASS $suite " || {
		echo "test: missouri suite $suite did not pass." >&2
		exit 1
	}
done

# Not hermetic on purpose: the fake-gaff suites prove kersh's hook
# logic, and this proves kersh's emitted vocabulary matches the real
# gaff's generic host. The missouri setup built the fake-model binary.
command -v gaff >/dev/null || {
	echo "test: gaff is not on PATH, so the seam cannot run." >&2
	echo "  cargo install --locked --git https://github.com/cjohnhanson/gaff" >&2
	exit 1
}
echo "test: seam against real gaff"
sh tests/seam_real_gaff.sh "$(command -v gaff)" "$(pwd)/target/debug/kersh"
echo "test: ok"
