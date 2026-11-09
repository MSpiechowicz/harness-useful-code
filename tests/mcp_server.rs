//! The MCP server of the real executable: the handshake, the tool list, and what it answers to what it cannot
//! serve.

mod common;

use common::stdio::{Client, command};
use serde_json::json;
use tempfile::TempDir;

fn started(dir: &TempDir, vars: &[(&str, &str)]) -> Client {
    let mut command = command(&dir.path().join("data"), dir.path());

    command.env("HOME", dir.path()).env_remove("HARNESS_CODE_INDEX");

    for (name, value) in vars {
        command.env(name, value);
    }

    Client::start(command)
}

#[test]
fn the_handshake_names_the_server_and_points_at_the_code_tools() {
    let dir = TempDir::new().expect("temp");
    let client = started(&dir, &[]);
    let initialized = &client.initialized;

    assert_eq!(initialized["serverInfo"]["name"], "harness-code");
    assert!(
        initialized["serverInfo"]["version"]
            .as_str()
            .expect("version")
            .ends_with("-dev"),
        "{initialized}"
    );
    assert_eq!(
        initialized["capabilities"],
        json!({ "tools": { "listChanged": false } })
    );

    let instructions = initialized["instructions"].as_str().expect("instructions");

    assert!(
        instructions.contains("code_map, code_symbol, code_impact, code_path"),
        "{instructions}"
    );
    assert!(
        instructions.chars().count() <= 2000,
        "{} characters",
        instructions.chars().count()
    );
}

#[test]
fn tools_list_snapshot() {
    let dir = TempDir::new().expect("temp");
    let mut client = started(&dir, &[]);
    let reply = client.request("tools/list", json!({}));

    insta::assert_json_snapshot!(reply["result"]);

    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();

    assert_eq!(names, ["code_map", "code_symbol", "code_impact", "code_path"]);

    for tool in reply["result"]["tools"].as_array().expect("tools") {
        assert_eq!(tool["_meta"]["anthropic/alwaysLoad"], true, "{tool}");
    }
}

#[test]
fn no_tool_is_listed_while_the_switch_is_off() {
    let dir = TempDir::new().expect("temp");
    let mut client = started(&dir, &[("HARNESS_CODE_INDEX", "0")]);
    let reply = client.request("tools/list", json!({}));

    assert_eq!(reply["result"], json!({ "tools": [] }));
    assert!(
        !client.initialized["instructions"]
            .as_str()
            .expect("instructions")
            .contains("code_map"),
    );
}

#[test]
fn a_tool_that_does_not_exist_and_a_method_that_does_not_exist_are_errors() {
    let dir = TempDir::new().expect("temp");
    let mut client = started(&dir, &[]);

    let unknown = client.request("tools/call", json!({ "name": "note_search", "arguments": {} }));
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    let nameless = client.request("tools/call", json!({}));
    assert_eq!(nameless["error"]["code"], -32602, "{nameless}");

    let method = client.request("resources/list", json!({}));
    assert_eq!(method["error"]["code"], -32601, "{method}");

    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn a_call_without_a_project_path_in_the_home_folder_is_a_tool_error_that_says_what_is_missing() {
    let dir = TempDir::new().expect("temp");
    let mut client = started(&dir, &[]);
    let reply = client.request("tools/call", json!({ "name": "code_map", "arguments": {} }));

    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("project_path"),
        "{reply}"
    );
}

#[test]
fn an_oversized_line_and_broken_json_are_answered_and_serving_goes_on() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let dir = TempDir::new().expect("temp");
    let mut child = command(&dir.path().join("data"), dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    writeln!(stdin, "{{ not json").expect("write");
    writeln!(stdin, "[]").expect("write");
    writeln!(stdin, "{}", "x".repeat(2 * 1024 * 1024)).expect("write");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":9,"method":"ping"}}"#).expect("write");
    drop(stdin);

    let mut codes = Vec::new();
    let mut line = String::new();

    while stdout.read_line(&mut line).expect("read") > 0 {
        let reply: serde_json::Value = serde_json::from_str(&line).expect("JSON");

        codes.push(reply["error"]["code"].as_i64().unwrap_or(0));
        line.clear();
    }

    assert_eq!(codes, [-32700, -32600, -32600, 0]);
    assert!(child.wait().expect("exits").success());
}
