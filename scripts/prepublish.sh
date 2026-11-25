#!/bin/sh
# Everything that must be true before this crate publishes. Run it before
# pushing a release tag.
#
# One feature set is not a feature matrix: a consumer can take the crate
# with any combination of features, so cargo-hack walks each feature
# alone and the powerset. A registry keeps a published version forever,
# so every check runs while the version can still change.
set -eu
[ -f rust-toolchain.toml ] || {
	echo "  prepublish: run this from the repository root, where rust-toolchain.toml is." >&2
	exit 1
}
# The toolchain rust-toolchain.toml pins, so the version has one home.
TC="${TOOLCHAIN:-$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)}"
fail=0
say() { printf '  %-46s %s\n' "$1" "$2"; }

# A refusal prints why, so a full disk and a packaging fault read apart.
run() {
	label="$1"
	shift
	# The assignment sits in the `if` test, where `set -e` does not fire.
	if out=$("$@" 2>&1); then
		say "$label" ok
	else
		say "$label" FAILS
		printf '%s\n' "$out" | tail -12 | sed 's/^/      /'
		fail=1
	fi
}

# Every program the checks call, named before the first check runs.
for tool in cargo-hack cargo-audit rustup python3 curl; do
	case "$tool" in
	cargo-*) cargo "${tool#cargo-}" --version >/dev/null 2>&1 && continue ;;
	*) command -v "$tool" >/dev/null 2>&1 && continue ;;
	esac
	case "$tool" in
	cargo-*) echo "  prepublish: $tool is missing. cargo install $tool" >&2 ;;
	rustup) echo "  prepublish: rustup is missing. https://rustup.rs" >&2 ;;
	*) echo "  prepublish: $tool is missing. Install it with the system package manager." >&2 ;;
	esac
	exit 1
done

# --no-dev-deps and --all-targets are mutually exclusive in cargo-hack.
run "check, every feature alone" rustup run "$TC" cargo hack check --each-feature --no-dev-deps
run "check, every feature, tests" rustup run "$TC" cargo hack check --each-feature --all-targets
run "check, the feature powerset" rustup run "$TC" cargo hack check --feature-powerset --no-dev-deps
run "test --all-features" rustup run "$TC" cargo test --all-features
run "clippy" rustup run "$TC" cargo clippy --all-targets --all-features -- -D warnings
run "fmt" rustup run "$TC" cargo fmt --check
run "audit" cargo audit --deny yanked --deny warnings

# A clean checkout is what a runner and a consumer both get.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git archive HEAD | tar -x -C "$tmp"
run "clean checkout resolves" sh -c "cd '$tmp' && rustup run '$TC' cargo metadata --format-version 1"
run "publish dry run" sh -c "cd '$tmp' && rustup run '$TC' cargo publish --locked --dry-run --allow-dirty"

# A version already on the registry cannot be republished. Only a 404
# means the version is free; any other answer refuses.
meta=$(rustup run "$TC" cargo metadata --no-deps --format-version 1)
name=$(printf '%s' "$meta" | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["name"])')
ver=$(printf '%s' "$meta" | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
code=$(curl -s -o /dev/null -w '%{http_code}' \
	-H "User-Agent: $name-prepublish (https://github.com/cjohnhanson/$name)" \
	"https://crates.io/api/v1/crates/$name/$ver" 2>/dev/null || echo 000)
case "$code" in
404) say "version $ver is unpublished" ok ;;
200)
	say "version $ver is unpublished" "FAILS (already published)"
	fail=1
	;;
*)
	say "version $ver is unpublished" "FAILS (crates.io answered $code)"
	fail=1
	;;
esac

[ "$fail" -eq 0 ] && echo "  PRE-PUBLISH: ok" || echo "  PRE-PUBLISH: REFUSED"
exit "$fail"
