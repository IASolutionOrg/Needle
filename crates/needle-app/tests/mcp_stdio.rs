use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn mcp_answers_before_stdin_eof() {
    let binary = env!("CARGO_BIN_EXE_needle");
    let mut child = Command::new(binary)
        .args(["mcp", "serve-benchmark"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn MCP server");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    writeln!(stdin, "not-json").expect("write malformed request");
    stdin.flush().expect("flush malformed request");
    let mut parse_line = String::new();
    reader.read_line(&mut parse_line).expect("read parse error");
    let parse_error: serde_json::Value =
        serde_json::from_str(&parse_line).expect("parse error JSON");
    assert_eq!(parse_error["error"]["code"], -32700);
    assert!(parse_error["id"].is_null());
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
    writeln!(stdin, "{initialize}").expect("write initialize");
    stdin.flush().expect("flush initialize");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read initialize response");
    let initialized: serde_json::Value = serde_json::from_str(&line).expect("initialize JSON");
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    writeln!(stdin, "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}")
        .expect("write initialized notification");
    stdin.flush().expect("flush initialized notification");
    for request in [
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"progressToken":0}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"need_context","arguments":{"route":"locate.implementation","subject":{"kind":"symbol","name":"answer"},"required":[],"preferred":[],"world":{"source":"current","platform":"current","features":"default"},"task":"Locate answer."},"_meta":{"progressToken":1}}}"#,
    ] {
        writeln!(stdin, "{request}").expect("write request");
        stdin.flush().expect("flush request");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read response");
        assert!(!line.trim().is_empty(), "response arrived before EOF");
        let value: serde_json::Value = serde_json::from_str(&line).expect("JSON response");
        assert_eq!(value["id"], request_id(request));
        if value["id"] == 3 {
            assert_eq!(
                value["result"]["content"][0]["text"],
                value["result"]["structuredContent"]["context"]
            );
        }
    }
    drop(stdin);
    assert!(child.wait().expect("wait MCP server").success());
}

fn request_id(request: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(request).expect("request JSON")["id"].clone()
}
