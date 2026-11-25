//! REST on the host's `host.sock`, end to end: the `OpenAPI` document, and
//! the session routes answering what the matching commands print with
//! `--json`. No TCP listener exists anywhere: the host, the session
//! server, and `kersh serve` hold no inet socket.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::Env;

/// This build's version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A stand-in host: a `host.lock` naming `schema` and `version`, and a
/// listener on `host.sock` that answers each request with `reply`. Its
/// pid is a `sleep` child, so a test can stop it as a user would.
struct FakeHost {
    child: Child,
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: std::path::PathBuf,
}

impl FakeHost {
    fn start(env: &Env, schema: u32, version: &str, reply: String) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(env.state()).unwrap();
        std::fs::set_permissions(env.state(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = Command::new("/bin/sleep").arg("600").spawn().unwrap();
        std::fs::write(
            env.state().join("host.lock"),
            json!({"pid": child.id(), "entry": ["kersh"], "version": version, "schema": schema})
                .to_string(),
        )
        .unwrap();
        let socket = env.state().join("host.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let thread = {
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    // Read the request head: a JSON line, or HTTP's
                    // header block.
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while let Ok(1) = stream.read(&mut byte) {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") || (head.ends_with(b"\n") && head[0] == b'{')
                        {
                            break;
                        }
                    }
                    if head.is_empty() {
                        continue;
                    }
                    requests.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(reply.as_bytes());
                }
            })
        };
        Self {
            child,
            stop,
            requests,
            thread: Some(thread),
            socket,
        }
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).unwrap()
    }

    /// The requests that reached it.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Stop listening. The socket file stays, as a killed host's does.
    fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_host_of_the_old_schema_is_refused_naming_both_versions_and_the_fix() {
    let env = Env::new();
    // A schema 1 host reads one JSON line per request and answers one.
    let mut old = FakeHost::start(
        &env,
        1,
        "0.6.0",
        "{\"error\":\"not a request\"}\n".to_owned(),
    );
    let pid = old.pid();
    let output = env.kersh(&["stop", "acme1"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        support::stderr(&output)
    );
    let text = support::stderr(&output);
    for needle in [
        "kersh 0.6.0",
        "schema 1",
        &format!("kersh {VERSION}"),
        "schema 2",
        &format!("`kill {pid}`"),
        "the next kersh command starts a host of this build",
    ] {
        assert!(text.contains(needle), "`{needle}` is missing: {text}");
    }
    assert_eq!(old.requests(), 0, "a request reached the old host");

    // The step the refusal names: stop the old host, and the next
    // command starts one of this build.
    support::kill(pid, "TERM");
    let _ = old.child.wait();
    old.close();
    let output = env.kersh(&["stop", "acme1"]);
    let text = support::stderr(&output);
    assert!(text.contains("no session `acme1`"), "{text}");
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(env.state().join("host.lock")).unwrap())
            .unwrap();
    assert_eq!(lock["schema"], 2, "{lock}");
    assert_eq!(lock["version"], VERSION, "{lock}");
}

#[test]
fn a_response_without_this_schema_is_refused() {
    for (headers, found) in [
        ("", "has schema 0"),
        (
            "kersh-schema: 99\r\nkersh-version: 9.0.0\r\n",
            "kersh 9.0.0 has schema 99",
        ),
    ] {
        let env = Env::new();
        let host = FakeHost::start(
            &env,
            2,
            VERSION,
            format!("HTTP/1.1 204 No Content\r\n{headers}connection: close\r\n\r\n"),
        );
        let output = env.kersh(&["rm", "acme1"]);
        let text = support::stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{headers:?}: {text}");
        assert!(text.contains(found), "{headers:?}: {text}");
        assert_eq!(host.requests(), 1);
    }
}

#[test]
fn start_refuses_a_relative_cwd_and_an_unknown_field() {
    let env = Env::new();
    env.start_host();
    let client = http(&env);
    let work = env.work().display().to_string();
    for (body, needle) in [
        (json!({"harness": "claude-code"}), "absolute path"),
        // The host's own directory is `w`, so `.` would resolve there.
        (
            json!({"harness": "claude-code", "cwd": "."}),
            "absolute path",
        ),
        (
            json!({"harness": "claude-code", "cwd": work, "group": "acme", "max_runing": 1}),
            "max_runing",
        ),
    ] {
        let response = client.post(url("/v1/sessions")).json(&body).send().unwrap();
        assert_eq!(response.status(), 400, "{body}");
        let error: Value = response.json().unwrap();
        assert_eq!(error["code"], "usage", "{error:#}");
        assert!(
            error["message"].as_str().unwrap().contains(needle),
            "{body}: {error:#}"
        );
    }
    let listed: Value = client
        .get(url("/v1/sessions"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(listed, json!([]));
}

#[test]
fn changes_streams_the_records_that_read_and_logs_the_one_that_does_not() {
    let env = Env::new();
    env.start_host();
    let started = cli_lines(&env, &["start", "--harness", "claude-code", "--json"]);
    let id = started[0]["id"].as_str().unwrap().to_owned();
    // An ended session changes no more, so only the first read can
    // stream it.
    env.running_with_pid(&id);
    cli_lines(&env, &["stop", &id, "--json"]);
    let bad = env.state().join("sessions").join("acmebad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("record.json"), "{").unwrap();

    let client = reqwest::blocking::Client::builder()
        .unix_socket(env.state().join("host.sock"))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let response = client.get(url("/v1/changes")).send().unwrap();
    let mut reader = std::io::BufReader::new(response);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
    let first: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(first["id"], id.as_str(), "{line}");

    let deadline = Instant::now() + Duration::from_secs(20);
    while !env.host_log().contains("unreadable record") {
        assert!(Instant::now() < deadline, "{}", env.host_log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

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

    // Every route can refuse a request of another schema with a 400, and
    // a start can fail with a 500.
    for (path, item) in document["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            assert!(
                operation["responses"]["400"].is_object(),
                "{method} {path} lists no 400"
            );
        }
    }
    for path in ["/v1/sessions", "/v1/launches"] {
        assert!(
            document["paths"][path]["post"]["responses"]["500"].is_object(),
            "POST {path} lists no 500"
        );
    }

    // An event's raw line is JSON or, for a line that is not JSON, text.
    let raw = &document["components"]["schemas"]["Event"]["properties"]["raw"];
    assert!(
        raw.get("type").is_none_or(|kind| kind
            .as_array()
            .is_some_and(|kinds| kinds.contains(&json!("string")))),
        "Event.raw cannot be a string: {raw}"
    );

    // `cwd` says what POST /v1/sessions requires.
    let cwd = document["components"]["schemas"]["StartRequest"]["properties"]["cwd"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(cwd.contains("POST /v1/sessions"), "{cwd}");
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

fn lsof(args: &[&str]) -> std::process::Output {
    Command::new("lsof")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("this check needs lsof on PATH: {error}"))
}

/// The inet sockets a process holds, as `lsof` lists them. lsof must
/// first list the process's other files, or an empty answer proves
/// nothing.
fn inet_sockets(pid: i32) -> String {
    let pid = pid.to_string();
    let all = lsof(&["-a", "-n", "-P", "-p", &pid]);
    let listed = String::from_utf8_lossy(&all.stdout);
    assert!(
        listed
            .lines()
            .skip(1)
            .any(|line| line.split_whitespace().nth(1) == Some(pid.as_str())),
        "lsof lists no file of pid {pid}, so it cannot show its sockets: {listed}{}",
        String::from_utf8_lossy(&all.stderr)
    );
    let inet = lsof(&["-a", "-n", "-P", "-i", "-p", &pid]);
    let sockets = String::from_utf8_lossy(&inet.stdout).into_owned();
    // lsof exits 1 when nothing matches.
    assert!(
        inet.status.success() || (inet.status.code() == Some(1) && sockets.trim().is_empty()),
        "lsof -i failed for pid {pid}: {}",
        String::from_utf8_lossy(&inet.stderr)
    );
    sockets
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
        let sockets = inet_sockets(pid);
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
