#!/usr/bin/env node
// The binary lives in a per-platform package under optionalDependencies,
// so a package manager installs only the one that matches. This resolves
// it through node's own lookup rather than a hardcoded node_modules path,
// and runs it with this process's terminal.
//
// Nothing is downloaded at install time. There is no postinstall, so an
// offline install, an air-gapped runner, and `--ignore-scripts` all work.
const { platform, arch, env } = process;
const { spawnSync } = require("child_process");
const { existsSync } = require("fs");
const os = require("os");

// The Linux binaries are static musl, which runs on a musl host and on
// a glibc host alike. One Linux entry serves both.
const PLATFORMS = {
	darwin: {
		arm64: "@cjohnhanson/kersh-darwin-arm64/kersh",
		x64: "@cjohnhanson/kersh-darwin-x64/kersh",
	},
	linux: {
		arm64: "@cjohnhanson/kersh-linux-arm64-musl/kersh",
		x64: "@cjohnhanson/kersh-linux-x64-musl/kersh",
	},
};

const rel = env.KERSH_BINARY ? null : PLATFORMS?.[platform]?.[arch];
let bin = env.KERSH_BINARY || null;
let unresolved = false;
if (!bin && rel) {
	// The platform is built, so a failure here means the package did not
	// install, which gets its own message.
	try {
		bin = require.resolve(rel);
	} catch {
		unresolved = true;
	}
}

if (bin && !existsSync(bin)) {
	console.error(`KERSH_BINARY is set to ${bin}, and no file is there.`);
	process.exitCode = 1;
} else if (unresolved) {
	console.error(
		`kersh supports ${platform} ${arch}, and its binary package is not ` +
			"installed. Reinstall with `npm install --include=optional`.",
	);
	process.exitCode = 1;
} else if (!bin) {
	console.error(
		`kersh ships no prebuilt binary for ${platform} ${arch}. ` +
			"Install it with `cargo install kersh`, or set KERSH_BINARY to a path.",
	);
	process.exitCode = 1;
} else {
	const result = spawnSync(bin, process.argv.slice(2), {
		shell: false,
		stdio: "inherit",
	});
	if (result.error) {
		console.error(`kersh cannot run ${bin}: ${result.error.message}`);
		process.exitCode = 1;
	} else if (result.signal) {
		// A binary killed by a signal has a null status, which would exit
		// 0. Report it the way a shell does, as 128 plus the signal number.
		const number = os.constants.signals[result.signal];
		console.error(`kersh: ${bin} was killed by ${result.signal}`);
		process.exitCode = number ? 128 + number : 1;
	} else {
		process.exitCode = result.status;
	}
}
