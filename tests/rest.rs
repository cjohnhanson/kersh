//! REST on the host's `host.sock`, end to end: the `OpenAPI` document, and
//! the session routes answering what the matching commands print with
//! `--json`. No TCP listener exists anywhere: the host, the session
//! server, and `kersh serve` hold no inet socket.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::collections::BTreeSet;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::Env;

fn http(env: &Env) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .unix_socket(env.state().join("host.sock"))
        .build()
        .unwrap()
}

fn url(path: &str) -> String {
    format!("http://kersh{path}")
}

fn cli_lines(env: &Env, args: &[&str]) -> Vec<Value> {
    let output = env.kersh(args);
    assert!(output.status.success(), "{}", support::stderr(&output));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Every `$ref` in `value`.
fn refs(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "$ref"
                    && let Some(text) = item.as_str()
                {
                    found.push(text.to_owned());
                }
                refs(item, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| refs(item, found)),
        _ => {}
    }
}

#[test]
fn the_openapi_document_is_served_and_valid() {
    let env = Env::new();
    env.start_host();
    let response = http(&env).get(url("/openapi.json")).send().unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().unwrap();
    let document: Value = serde_json::from_str(&text).unwrap();
    assert!(
        document["openapi"].as_str().unwrap().starts_with("3.1"),
        "{document:#}"
    );
    assert_eq!(document["info"]["title"], "kersh");

    // It parses as an OpenAPI 3.1 document.
    oas3::from_json(&text).unwrap();

    // Every reference names a schema the document defines.
    let mut found = Vec::new();
    refs(&document, &mut found);
    assert!(!found.is_empty());
    for reference in found {
        let name = reference
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("{reference} is not a schema reference"));
        assert!(
            document["components"]["schemas"][name].is_object(),
            "{reference} names no schema"
        );
    }

    // The session commands are each a route.
    let paths: BTreeSet<&str> = document["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for path in [
        "/v1/sessions",
        "/v1/sessions/{id}",
        "/v1/sessions/{id}/stop",
        "/v1/sessions/{id}/resume",
        "/v1/sessions/{id}/restart",
        "/v1/sessions/{id}/send",
        "/v1/sessions/{id}/events",
        "/v1/sessions/{id}/screen",
        "/v1/changes",
        "/v1/harnesses",
        "/v1/runtimes",
        "/v1/runtimes/{name}",
    ] {
        assert!(paths.contains(path), "{path} is missing: {paths:?}");
    }
}

#[test]
fn sessions_and_start_equal_the_cli_json() {
    let env = Env::new();
    env.start_host();
    let client = http(&env);

    let listed: Value = client
        .get(url("/v1/sessions"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(listed, json!([]));

    let response = client
        .post(url("/v1/sessions"))
        .json(&json!({
            "harness": "claude-code",
            "cwd": env.work(),
            "name": "from-rest",
            "labels": {"card": "acme-2"}
        }))
        .send()
        .unwrap();
    assert_eq!(response.status(), 201);
    let started: Value = response.json().unwrap();
    let id = started["id"].as_str().unwrap().to_owned();

    // The record has the shape `kersh start --json` prints.
    let cli = cli_lines(
        &env,
        &[
            "start",
            "--harness",
            "claude-code",
            "--name",
            "from-cli",
            "--json",
        ],
    );
    let keys = |value: &Value| -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    };
    assert_eq!(keys(&started), keys(&cli[0]));
    assert_eq!(started["name"], "from-rest");
    assert_eq!(started["labels"]["card"], "acme-2");
    assert_eq!(started["state"], "running");

    env.running_with_pid(&id);
    env.running_with_pid(cli[0]["id"].as_str().unwrap());
    let listed: Value = client
        .get(url("/v1/sessions"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(listed, Value::Array(cli_lines(&env, &["ps", "--json"])));
    let one: Value = client
        .get(url(&format!("/v1/sessions/{id}")))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(vec![one], cli_lines(&env, &["ps", &id, "--json"]));

    // An error carries the code the CLI's exit status comes from.
    let response = client.get(url("/v1/sessions/nosuch")).send().unwrap();
    assert_eq!(response.status(), 404);
    let error: Value = response.json().unwrap();
    assert_eq!(error["code"], "not_found");

    let stopped: Value = client
        .post(url(&format!("/v1/sessions/{id}/stop")))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(stopped["state"], "ended");
    assert_eq!(cli_lines(&env, &["ps", &id, "--json"]), vec![stopped]);
}

#[test]
fn another_protocol_schema_is_refused_naming_both_versions() {
    let env = Env::new();
    env.start_host();
    let response = http(&env)
        .get(url("/v1/sessions"))
        .header("kersh-schema", "99")
        .header("kersh-version", "9.0.0")
        .send()
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.headers().get("kersh-schema").is_some());
    let error: Value = response.json().unwrap();
    assert_eq!(error["code"], "schema", "{error:#}");
    let message = error.to_string();
    assert!(message.contains("9.0.0"), "{message}");
}

/// The inet sockets a process holds, as `lsof` lists them.
fn inet_sockets(pid: i32) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-a", "-n", "-P", "-i", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn no_process_listens_on_tcp() {
    let env = Env::new();
    env.start_host();
    let mut serve = env
        .command(&["serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // A start brings up the session server too.
    cli_lines(&env, &["start", "--harness", "claude-code", "--json"]);
    assert!(
        serve.try_wait().unwrap().is_none(),
        "kersh serve exited before it was checked"
    );
    let serve_pid = i32::try_from(serve.id()).unwrap();
    for (what, pid) in [
        ("the host", env.host_pid().unwrap()),
        ("the session server", env.mux_pid().unwrap()),
        ("kersh serve", serve_pid),
    ] {
        let Some(sockets) = inet_sockets(pid) else {
            eprintln!("lsof is not installed; the source check covers TCP");
            break;
        };
        assert!(sockets.trim().is_empty(), "{what} holds {sockets}");
    }
    let _ = serve.kill();
    let _ = serve.wait();
}

#[test]
fn no_source_file_opens_a_tcp_listener() {
    let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    let mut offenders = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for needle in ["TcpListener", "TcpSocket", "UdpSocket", "SocketAddr"] {
                if text.contains(needle) {
                    offenders.push(format!("{}: {needle}", path.display()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}
