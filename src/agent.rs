//! MCP 2025-11-25 stdio adapter. Only pre-authorized sessions can execute.
use crate::{
    client,
    protocol::{
        Failure, InputMode, Origin, Request, Response, random_id, read_frame, validate_id,
        write_frame,
    },
};
use serde_json::{Value, json};
use std::{
    io::{self, BufReader},
    path::Path,
    time::Instant,
};

pub fn serve(runtime: &Path) -> Result<(), Failure> {
    let mut input = BufReader::new(io::stdin());
    let mut output = io::stdout();
    let mut initialized = false;
    let mut agent = String::from("mcp");
    loop {
        let request: Value = match read_frame(&mut input) {
            Ok(value) => value,
            Err(error) if error.code == "disconnected" => return Ok(()),
            Err(error) => {
                write_frame(
                    &mut output,
                    &rpc_error(&Value::Null, -32700, "Invalid JSON-RPC input."),
                )?;
                // Oversized frames can leave unread bytes before their newline.
                // Never interpret that rejected frame's tail as another request.
                return Err(error);
            }
        };
        if !request.is_object() {
            write_frame(
                &mut output,
                &rpc_error(&Value::Null, -32600, "Invalid JSON-RPC request."),
            )?;
            continue;
        }
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        if !(id.is_string() || id.is_i64() || id.is_u64()) {
            write_frame(
                &mut output,
                &rpc_error(&Value::Null, -32600, "Invalid JSON-RPC identifier."),
            )?;
            continue;
        }
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let result = if request.get("jsonrpc").and_then(Value::as_str) == Some("2.0") {
            match method {
                "initialize" => {
                    initialized = true;
                    request
                        .pointer("/params/clientInfo/name")
                        .and_then(Value::as_str)
                        .filter(|name| validate_id(name).is_ok())
                        .unwrap_or("mcp")
                        .clone_into(&mut agent);
                    Ok(
                        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"latchrun","version":env!("CARGO_PKG_VERSION")},"instructions":"Execute only within operator-created sessions. Supply a unique operation ID. Never replay a lost response; inspect status. Tools return redacted output and no credentials."}),
                    )
                }
                "ping" => Ok(json!({})),
                "tools/list" if initialized => Ok(json!({"tools":tools()})),
                "tools/call" if initialized => Ok(call(
                    runtime,
                    &agent,
                    request.get("params").unwrap_or(&Value::Null),
                )),
                "tools/list" | "tools/call" => {
                    Err((-32000, "Initialize the MCP connection first."))
                }
                _ => Err((
                    -32601,
                    "Method not supported; this adapter supports MCP 2025-11-25.",
                )),
            }
        } else {
            Err((-32600, "Invalid JSON-RPC request."))
        };
        let response = result.map_or_else(
            |(code, message)| rpc_error(&id, code, message),
            |result| json!({"jsonrpc":"2.0","id":id,"result":result}),
        );
        write_frame(&mut output, &response)?;
    }
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn tools() -> Vec<Value> {
    let mut tools = Vec::new();
    for (name, description, required) in [
        (
            "latchrun_status",
            "Inspect sessions and operation outcomes; never retries work.",
            false,
        ),
        (
            "latchrun_events",
            "Read secret-free mediated activity metadata.",
            false,
        ),
        (
            "latchrun_inspect",
            "Inspect declared environment provenance without fetching secrets.",
            true,
        ),
        ("latchrun_stop", "Stop a session and its commands.", true),
        (
            "latchrun_refresh",
            "Clear cached credentials; future runs reauthorize.",
            true,
        ),
    ] {
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":{"session":{"type":"string"}},"required":if required{vec!["session"]}else{vec![]},"additionalProperties":false},"annotations":{"readOnlyHint":matches!(name,"latchrun_status"|"latchrun_events"|"latchrun_inspect"),"openWorldHint":false}}));
    }
    tools.push(json!({"name":"latchrun_run","description":"Run one exact approved command in an existing session. Operation IDs are durable and never replayed. stdin is null; output is redacted and bounded.","inputSchema":{"type":"object","properties":{"session":{"type":"string"},"operation":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1}},"required":["session","operation","argv"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}}));
    tools.push(json!({"name":"latchrun_analytics","description":"Inspect durable aggregate usage for the selected number of days. No commands, output or credentials are included.","inputSchema":{"type":"object","properties":{"days":{"type":"integer","enum":[1,7,30,90],"default":7}},"additionalProperties":false},"annotations":{"readOnlyHint":true,"openWorldHint":false}}));
    tools
}

fn call(runtime: &Path, agent: &str, params: &Value) -> Value {
    let started = Instant::now();
    let result = invoke(runtime, params);
    let telemetry_failed = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| advertised_tool(name))
        .is_some_and(|tool| {
            let success = result.as_ref().is_ok_and(|value| {
                value
                    .get("exit_code")
                    .is_none_or(|code| code.as_i64() == Some(0))
            });
            record_activity(runtime, agent, tool, started, success).is_err()
        });
    let mut response = match result {
        Ok(value) => {
            json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
        }
        Err(error) => json!({"content":[{"type":"text","text":error.to_string()}],"isError":true}),
    };
    if telemetry_failed {
        const WARNING: &str = "Usage recording was unavailable. The tool outcome above is unchanged; do not repeat a command to recover telemetry.";
        response["_meta"] = json!({"telemetry_warning": WARNING});
        if let Some(content) = response["content"].as_array_mut() {
            content.push(json!({"type":"text","text":WARNING}));
        }
    }
    response
}

fn advertised_tool(name: &str) -> bool {
    matches!(
        name,
        "latchrun_status"
            | "latchrun_events"
            | "latchrun_inspect"
            | "latchrun_stop"
            | "latchrun_refresh"
            | "latchrun_run"
            | "latchrun_analytics"
    )
}

fn record_activity(
    runtime: &Path,
    agent: &str,
    tool: &str,
    started: Instant,
    success: bool,
) -> Result<(), Failure> {
    let query = Request::Activity {
        id: random_id()?,
        agent: agent.into(),
        tool: tool.into(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        success,
        source: "mcp".into(),
    };
    match client::request(runtime, &query)? {
        Response::Ok { .. } => Ok(()),
        Response::Error { code, message } => Err(Failure { code, message }),
        _ => Err(invalid()),
    }
}
fn invalid() -> Failure {
    Failure::new(
        "invalid_tool",
        "Invalid tool or arguments. Use the advertised tool schema.",
    )
}
fn invoke(runtime: &Path, params: &Value) -> Result<Value, Failure> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let session = arguments
        .get("session")
        .map(|value| value.as_str().ok_or_else(invalid))
        .transpose()?;
    if let Some(id) = session {
        validate_id(id)?;
    }
    let allowed = match name {
        "latchrun_run" => &["session", "operation", "argv"][..],
        "latchrun_analytics" => &["days"][..],
        _ => &["session"][..],
    };
    if arguments.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid());
    }
    let query = match name {
        "latchrun_analytics" => {
            let days = arguments
                .get("days")
                .map(|value| value.as_u64().ok_or_else(invalid))
                .transpose()?
                .unwrap_or(7);
            if !matches!(days, 1 | 7 | 30 | 90) {
                return Err(invalid());
            }
            Request::Analytics {
                days: u16::try_from(days).map_err(|_| invalid())?,
            }
        }
        "latchrun_status" => Request::Status {
            session: session.map(str::to_owned),
        },
        "latchrun_events" => Request::Events {
            session: session.map(str::to_owned),
        },
        "latchrun_inspect" => Request::Inspect {
            session: session.ok_or_else(invalid)?.into(),
        },
        "latchrun_stop" => Request::Stop {
            session: session.ok_or_else(invalid)?.into(),
        },
        "latchrun_refresh" => Request::Refresh {
            session: session.ok_or_else(invalid)?.into(),
        },
        "latchrun_run" => {
            let operation = arguments
                .get("operation")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            validate_id(operation)?;
            let argv = arguments
                .get("argv")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(invalid))
                .collect::<Result<Vec<_>, _>>()?;
            return run(
                runtime,
                &Request::Run {
                    session: session.ok_or_else(invalid)?.into(),
                    operation: operation.into(),
                    argv,
                    input: InputMode::Null,
                    origin: Origin::Mcp,
                },
            );
        }
        _ => return Err(invalid()),
    };
    match client::request(runtime, &query)? {
        Response::Ok { data } => Ok(data),
        Response::Error { code, message } => Err(Failure { code, message }),
        _ => Err(invalid()),
    }
}

fn run(runtime: &Path, query: &Request) -> Result<Value, Failure> {
    let mut stream = client::connect(runtime)?;
    write_frame(&mut stream, query)?;
    stream.set_read_timeout(None)?;
    let mut reader = BufReader::new(stream);
    let mut operation = String::new();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;
    loop {
        match read_frame(&mut reader)? {
            Response::Accepted { operation: id } => operation = id,
            Response::Output { stream, data } => {
                let target = if stream == "stdout" {
                    &mut stdout
                } else {
                    &mut stderr
                };
                let take = data.len().min(32768usize.saturating_sub(target.len()));
                target.extend_from_slice(&data[..take]);
                truncated |= take < data.len();
            }
            Response::Finished { exit_code } => {
                return Ok(
                    json!({"operation":operation,"exit_code":exit_code,"stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),"truncated":truncated}),
                );
            }
            Response::Error { code, message } => return Err(Failure { code, message }),
            Response::Ok { .. } => return Err(invalid()),
        }
    }
}
