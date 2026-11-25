//! The release workflow and its packaging files, read against each other.
//!
//! No release job runs before a tag, and a tag publishes to registries
//! that never take a version back. These tests parse the workflow and
//! check it against the files it names, so a rename fails here and not
//! on a tag. `actionlint` and `gaff check --github` run when they are on
//! `PATH`.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde_yaml_ng::Value;

const WORKFLOW: &str = ".github/workflows/release.yml";

const TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
];

fn read(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn workflow() -> Value {
    serde_yaml_ng::from_str(&read(WORKFLOW)).unwrap_or_else(|error| panic!("{WORKFLOW}: {error}"))
}

fn job<'a>(workflow: &'a Value, name: &str) -> &'a Value {
    workflow["jobs"]
        .get(name)
        .unwrap_or_else(|| panic!("no job {name} in {WORKFLOW}"))
}

/// Every `run:` script of a job, joined.
fn scripts(job: &Value) -> String {
    job["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|step| step.get("run").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn uses(job: &Value) -> Vec<String> {
    job["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|step| step.get("uses").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn targets(job: &Value) -> BTreeSet<String> {
    job["strategy"]["matrix"]["include"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|entry| entry["target"].as_str().unwrap().to_owned())
        .collect()
}

fn wanted() -> BTreeSet<String> {
    TARGETS.iter().map(|t| (*t).to_owned()).collect()
}

#[test]
fn a_version_tag_triggers_it_and_a_dispatch_rehearses_it() {
    let workflow = workflow();
    let on = &workflow["on"];
    let tags: Vec<&str> = on["push"]["tags"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(tags.contains(&"v[0-9]+.[0-9]+.[0-9]+"), "{tags:?}");
    assert!(on.get("workflow_dispatch").is_some());
    assert!(on["push"].get("branches").is_none());
    assert_eq!(workflow["permissions"]["contents"], "read");
}

#[test]
fn every_publish_job_runs_only_on_a_tag() {
    let workflow = workflow();
    for name in [
        "create-release",
        "publish-crate",
        "publish-pypi",
        "publish-npm",
        "finalize",
    ] {
        let condition = job(&workflow, name)["if"].as_str().unwrap_or_default();
        assert_eq!(condition, "github.event_name == 'push'", "{name}");
    }
}

/// A job's or step's `if`, without the `${{ }}` wrapper.
fn condition(value: &Value) -> Option<String> {
    let text = value.get("if")?.as_str()?.trim();
    let text = text
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .unwrap_or(text);
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn needs(job: &Value) -> Vec<String> {
    match job.get("needs") {
        None => Vec::new(),
        Some(Value::String(name)) => vec![name.clone()],
        Some(list) => list
            .as_sequence()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect(),
    }
}

/// The jobs that run for `event` when every job that runs succeeds, by
/// GitHub's rules: a job without a status function in its `if` runs
/// only when every job it needs, directly or not, succeeded; with
/// `!cancelled()` it runs whatever they did. Only the conditions this
/// workflow uses are read; any other fails the test, so a new one is
/// modelled before it is trusted.
fn jobs_that_run(workflow: &Value, event: &str) -> BTreeSet<String> {
    fn ran(
        workflow: &Value,
        event: &str,
        name: &str,
        memo: &mut std::collections::BTreeMap<String, bool>,
    ) -> bool {
        if let Some(known) = memo.get(name) {
            return *known;
        }
        let job = job(workflow, name);
        let direct = needs(job);
        let mut ancestors_ran = true;
        for need in &direct {
            ancestors_ran &=
                ran(workflow, event, need, memo) && all_ancestors(workflow, need, event, memo);
        }
        let mut status_function = false;
        let mut result = true;
        if let Some(text) = condition(job) {
            for clause in text.split("&&").map(str::trim) {
                if clause == "!cancelled()" {
                    status_function = true;
                } else if clause == "github.event_name == 'push'" {
                    result &= event == "push";
                } else if let Some(rest) = clause.strip_prefix("needs.") {
                    let need = rest
                        .strip_suffix(".result == 'success'")
                        .unwrap_or_else(|| panic!("job {name}: unread clause {clause:?}"));
                    assert!(
                        direct.iter().any(|d| d == need),
                        "job {name}: {need} is not in needs"
                    );
                    result &= ran(workflow, event, need, memo);
                } else {
                    panic!("job {name}: unread clause {clause:?}");
                }
            }
        }
        if !status_function {
            result &= ancestors_ran;
        }
        memo.insert(name.to_owned(), result);
        result
    }
    fn all_ancestors(
        workflow: &Value,
        name: &str,
        event: &str,
        memo: &mut std::collections::BTreeMap<String, bool>,
    ) -> bool {
        needs(job(workflow, name)).iter().all(|need| {
            ran(workflow, event, need, memo) && all_ancestors(workflow, need, event, memo)
        })
    }
    let mut memo = std::collections::BTreeMap::new();
    workflow["jobs"]
        .as_mapping()
        .unwrap()
        .keys()
        .map(|name| name.as_str().unwrap().to_owned())
        .filter(|name| ran(workflow, event, name, &mut memo))
        .collect()
}

#[test]
fn a_tag_runs_every_job() {
    let workflow = workflow();
    let all: BTreeSet<String> = workflow["jobs"]
        .as_mapping()
        .unwrap()
        .keys()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(jobs_that_run(&workflow, "push"), all);
}

#[test]
fn a_dispatch_runs_every_build_and_packaging_job_and_no_publish_job() {
    let ran = jobs_that_run(&workflow(), "workflow_dispatch");
    let expected: BTreeSet<String> = [
        "verify",
        "build",
        "wheels",
        "sdist",
        "npm-packages",
        "debs",
        "tap",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(ran, expected);
}

/// Whether a step sends anything outside the run: a release, a
/// registry, a provenance attestation, or another repository.
fn publishes(step: &Value) -> bool {
    let run = step.get("run").and_then(Value::as_str).unwrap_or_default();
    let action = step.get("uses").and_then(Value::as_str).unwrap_or_default();
    [
        "gh release",
        "npm publish",
        "cargo publish",
        "uv publish",
        "git push",
    ]
    .iter()
    .any(|word| run.contains(word))
        || action.starts_with("actions/attest-build-provenance")
        || step
            .get("with")
            .and_then(|with| with.get("token"))
            .is_some()
}

#[test]
fn no_step_of_a_dispatch_publishes() {
    let workflow = workflow();
    let ran = jobs_that_run(&workflow, "workflow_dispatch");
    let mut checked = 0;
    for name in &ran {
        for step in job(&workflow, name)["steps"].as_sequence().unwrap() {
            if publishes(step) {
                checked += 1;
                let gate = condition(step).unwrap_or_default();
                assert!(
                    gate.split("&&")
                        .any(|clause| clause.trim() == "github.event_name == 'push'"),
                    "job {name}: a publishing step runs on a dispatch: {step:?}"
                );
            }
        }
    }
    // build: attest, upload; debs: attach; tap: tap checkout, push.
    assert_eq!(checked, 5);
}

fn step<'a>(job: &'a Value, name: &str) -> &'a Value {
    job["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|step| step.get("name").and_then(Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("no step {name:?}"))
}

#[test]
fn the_tag_checks_run_on_every_tag_push() {
    let workflow = workflow();
    let verify = job(&workflow, "verify");
    assert_eq!(condition(verify), None, "verify runs on every event");
    for name in [
        "Check the tag is on main",
        "Check the tag matches Cargo.toml",
    ] {
        assert_eq!(
            condition(step(verify, name)).as_deref(),
            Some("github.event_name == 'push'"),
            "{name}"
        );
    }
    assert!(
        step(verify, "Check the tag is on main")["run"]
            .as_str()
            .unwrap()
            .contains("merge-base --is-ancestor")
    );
}

#[test]
fn the_build_runs_only_after_verify_passes() {
    let workflow = workflow();
    let build = job(&workflow, "build");
    assert_eq!(
        condition(build).as_deref(),
        Some("!cancelled() && needs.verify.result == 'success'")
    );
    assert_eq!(
        condition(step(build, "Attest the build provenance")).as_deref(),
        Some("github.event_name == 'push'")
    );
}

#[test]
fn the_packaging_jobs_run_after_the_build_whatever_create_release_did() {
    let workflow = workflow();
    for name in ["npm-packages", "debs"] {
        assert_eq!(
            condition(job(&workflow, name)).as_deref(),
            Some("!cancelled() && needs.build.result == 'success'"),
            "{name}"
        );
    }
}

#[test]
fn the_tap_pushes_only_after_every_publish() {
    let workflow = workflow();
    let tap = job(&workflow, "tap");
    let gate = condition(step(tap, "Push the formula")).unwrap();
    let clauses: BTreeSet<&str> = gate.split("&&").map(str::trim).collect();
    let expected: BTreeSet<&str> = [
        "github.event_name == 'push'",
        "needs.publish-crate.result == 'success'",
        "needs.publish-pypi.result == 'success'",
        "needs.publish-npm.result == 'success'",
        "needs.finalize.result == 'success'",
    ]
    .into_iter()
    .collect();
    assert_eq!(clauses, expected);
}

#[test]
fn the_tag_must_match_the_manifest_and_be_on_main() {
    let verify = scripts(job(&workflow(), "verify"));
    assert!(verify.contains("merge-base --is-ancestor"), "{verify}");
    assert!(verify.contains("does not match Cargo.toml"), "{verify}");
    assert!(verify.contains("sh scripts/test.sh"), "{verify}");
    assert!(verify.contains("cargo package --locked"), "{verify}");
}

#[test]
fn it_builds_macos_and_linux_on_both_architectures() {
    let workflow = workflow();
    assert_eq!(targets(job(&workflow, "build")), wanted());
    assert_eq!(targets(job(&workflow, "wheels")), wanted());
}

#[test]
fn each_archive_carries_its_checksum() {
    let workflow = workflow();
    let build = job(&workflow, "build");
    let script = scripts(build);
    assert!(
        script.contains("shasum -a 256 \"$staging.tar.gz\""),
        "{script}"
    );
    let text = serde_yaml_ng::to_string(build).unwrap();
    assert!(text.contains("outputs.asset }}.sha256"), "{text}");
    assert!(
        scripts(job(&workflow, "tap")).contains(".tar.gz.sha256"),
        "the tap reads no checksum"
    );
}

#[test]
fn the_toolchain_matches_rust_toolchain_toml() {
    let pinned = read("rust-toolchain.toml")
        .lines()
        .find_map(|line| line.strip_prefix("channel = \""))
        .map(|rest| rest.trim_end_matches('"').to_owned())
        .unwrap();
    let workflow = workflow();
    for (name, job) in workflow["jobs"].as_mapping().unwrap() {
        for action in uses(job) {
            if let Some(version) = action.strip_prefix("dtolnay/rust-toolchain@") {
                assert_eq!(version, pinned, "job {name:?}");
            }
        }
    }
}

#[test]
fn the_npm_generator_builds_every_target_the_workflow_builds() {
    let generator = read("npm/generate.mjs");
    let listed: BTreeSet<String> = generator
        .lines()
        .filter_map(|line| line.split("target: \"").nth(1))
        .map(|rest| rest.split('"').next().unwrap().to_owned())
        .collect();
    assert_eq!(listed, wanted());
    let package: serde_json::Value = serde_json::from_str(&read("npm/kersh/package.json")).unwrap();
    assert_eq!(package["name"], "kersh");
    assert_eq!(package["bin"]["kersh"], "bin/run.js");
    let optional: BTreeSet<&str> = package["optionalDependencies"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<&str> = [
        "@cjohnhanson/kersh-darwin-arm64",
        "@cjohnhanson/kersh-darwin-x64",
        "@cjohnhanson/kersh-linux-arm64-musl",
        "@cjohnhanson/kersh-linux-x64-musl",
    ]
    .into_iter()
    .collect();
    assert_eq!(optional, expected);

    let workflow = workflow();
    let packages = scripts(job(&workflow, "npm-packages"));
    assert!(packages.contains("node npm/generate.mjs kersh kersh bins"));
    let publish = scripts(job(&workflow, "publish-npm"));
    assert!(publish.contains("for d in ./npm/kersh-*;"), "{publish}");
    assert!(publish.contains("npm publish ./npm/kersh "), "{publish}");
}

#[test]
fn the_formula_placeholders_match_the_tap_job() {
    let formula = read("homebrew/kersh.rb");
    let tap = scripts(job(&workflow(), "tap"));
    let mut found = 0;
    for word in formula.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if word.starts_with("PLACEHOLDER_") {
            found += 1;
            assert!(tap.contains(word), "the tap job fills no {word}");
        }
    }
    assert_eq!(found, TARGETS.len());
    for target in TARGETS {
        assert!(formula.contains(&format!("kersh-v#{{version}}-{target}.tar.gz")));
        assert!(tap.contains(&format!("[{target}]=")), "{target}");
    }
}

#[test]
fn the_wheel_is_named_apart_from_the_command() {
    let pyproject = read("pyproject.toml");
    assert!(pyproject.contains("name = \"kersh-cli\""), "{pyproject}");
    assert!(pyproject.contains("bindings = \"bin\""), "{pyproject}");
}

fn on_path(program: &str) -> bool {
    Command::new("/bin/sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn actionlint_passes_when_installed() {
    if !on_path("actionlint") {
        eprintln!("actionlint is not on PATH; the YAML parse above stands in");
        return;
    }
    let output = Command::new("actionlint")
        .arg(WORKFLOW)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn gaff_check_github_passes_when_installed() {
    if !on_path("gaff") {
        eprintln!("gaff is not on PATH; skipped");
        return;
    }
    let output = Command::new("gaff")
        .args(["check", "--github"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
