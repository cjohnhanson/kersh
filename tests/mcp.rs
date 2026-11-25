//! `kersh serve`, the MCP server on stdio, end to end: a real session
//! server on a temp socket, fake harnesses on the login `PATH`, and the
//! host started by the first write. Each tool answers what the matching
//! command prints with `--json`.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod support;

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use support::Env;

/// The tools `kersh serve` offers: every command an agent can use without
/// a terminal, and not attach.
const TOOLS: &[&str] = &[
    "kersh_ps",
    "kersh_start",
    "kersh_stop",
    "kersh_resume",
    "kersh_restart",
    "kersh_send",
    "kersh_events",
    "kersh_screen",
    "kersh_agent_list",
    "kersh_agent_show",
    "kersh_runtime_list",
    "kersh_runtime_show",
    "kersh_harness_list",
    "kersh_setup",
];

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Server {
    fn start(env: &Env) -> Self {
        let mut child = env
            .command(&["serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn write(&mut self, message: &Value) {
        let mut line = serde_json::to_vec(message).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and wait for the response with its id.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no answer to {method}"));
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                assert!(
                    message.get("error").is_none(),
                    "{method} failed: {message:#}"
                );
                return message["result"].clone();
            }
        }
    }

    fn initialize(&mut self) -> Value {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "acme-client", "version": "0.0.1"}
            }),
        );
        self.write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        result
    }

    /// Call a tool and return its text and whether it reported an error.
    fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let text = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (text, result["isError"] == true)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn cli_lines(env: &Env, args: &[&str]) -> Vec<Value> {
    let output = env.kersh(args);
    assert!(output.status.success(), "{}", support::stderr(&output));
    lines(&String::from_utf8(output.stdout).unwrap())
}

#[test]
fn initialize_lists_every_tool_but_attach() {
    let env = Env::new();
    let mut server = Server::start(&env);
    let info = server.initialize();
    assert_eq!(info["serverInfo"]["name"], "kersh", "{info:#}");
    assert!(info["capabilities"]["tools"].is_object(), "{info:#}");

    let listed = server.request("tools/list", json!({}));
    let mut names: Vec<String> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    let mut expected: Vec<String> = TOOLS.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    assert_eq!(names, expected);
    for tool in listed["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool:#}");
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{tool:#}"
        );
    }
}

#[test]
fn ps_and_start_answer_what_the_commands_print() {
    let env = Env::new();
    let mut server = Server::start(&env);
    server.initialize();

    // No sessions yet, and ps needs no host.
    let (text, error) = server.call("kersh_ps", json!({}));
    assert!(!error, "{text}");
    assert!(lines(&text).is_empty(), "{text}");
    assert!(env.host_pid().is_none(), "a read started a host");

    let work = env.work().display().to_string();
    let (text, error) = server.call(
        "kersh_start",
        json!({
            "harness": "claude-code",
            "cwd": work,
            "name": "from-mcp",
            "labels": {"card": "acme-1"}
        }),
    );
    assert!(!error, "{text}");
    let started = lines(&text);
    assert_eq!(started.len(), 1, "{text}");
    let id = started[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(started[0]["name"], "from-mcp");
    assert_eq!(started[0]["labels"]["card"], "acme-1");
    assert_eq!(started[0]["state"], "running");

    env.running_with_pid(&id);
    let (text, error) = server.call("kersh_ps", json!({"ids": [id.clone()]}));
    assert!(!error, "{text}");
    assert_eq!(lines(&text), cli_lines(&env, &["ps", &id, "--json"]));

    // A failure comes back as a tool error naming the cause.
    let (text, error) = server.call("kersh_stop", json!({"id": "nosuch"}));
    assert!(error, "{text}");
    assert!(text.contains("nosuch"), "{text}");

    let (text, error) = server.call("kersh_stop", json!({"id": id}));
    assert!(!error, "{text}");
    assert_eq!(lines(&text)[0]["state"], "ended", "{text}");
}

#[test]
fn start_refuses_an_unknown_field() {
    let env = Env::new();
    let mut server = Server::start(&env);
    server.initialize();
    let (text, error) = server.call(
        "kersh_start",
        json!({
            "harness": "claude-code",
            "cwd": env.work().display().to_string(),
            "group": "acme",
            "max_runing": 1
        }),
    );
    assert!(error, "{text}");
    assert!(text.contains("max_runing"), "{text}");
    assert!(env.host_pid().is_none(), "the start went ahead");
}

#[test]
fn setup_reports_an_error_when_a_piece_is_missing() {
    // No gaff is on the test PATH, so `gaff init` cannot run.
    let env = Env::new();
    let mut server = Server::start(&env);
    server.initialize();
    let (text, error) = server.call("kersh_setup", json!({"harness": "claude-code"}));
    assert!(error, "{text}");
    assert!(text.contains("not fully wired"), "{text}");
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["piece"], "gaff init", "{text}");
    assert_eq!(first["present"], false, "{text}");
}

#[test]
fn reads_match_the_cli() {
    let env = Env::new();
    std::fs::write(
        env.home().join(".config/kersh/runtimes.yml"),
        "acme-fast:\n  harness: codex\n  mode: headless\n",
    )
    .unwrap();
    let mut server = Server::start(&env);
    server.initialize();

    let (text, error) = server.call("kersh_runtime_list", json!({}));
    assert!(!error, "{text}");
    assert_eq!(
        lines(&text),
        cli_lines(&env, &["runtime", "list", "--json"])
    );

    let (text, error) = server.call("kersh_runtime_show", json!({"name": "acme-fast"}));
    assert!(!error, "{text}");
    assert_eq!(
        lines(&text),
        cli_lines(&env, &["runtime", "show", "acme-fast", "--json"])
    );

    let (text, error) = server.call("kersh_agent_list", json!({}));
    assert!(!error, "{text}");
    assert_eq!(lines(&text), cli_lines(&env, &["list", "--json"]));
}
