mod api;
mod gui;
mod ops;
mod query;
mod scan;

use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};

fn help() {
    println!("DiskVibe {} — disk size inspector & cleaner\n", env!("CARGO_PKG_VERSION"));
    println!("  diskvibe [folder]            open the app (optionally scanning a folder)");
    println!("  diskvibe mcp                 run as an MCP server on stdio (for AI assistants)");
    println!("  diskvibe <tool> [path] [--option value]...\n\nTOOLS");
    for (name, desc, params) in api::TOOLS {
        println!("  {name:<9} {desc}");
        for (p, ty, req, d) in *params {
            println!("              --{p} <{ty}>{}  {d}", if *req { " (required)" } else { "" });
        }
    }
    println!("\n  every tool also takes  --format {}  and  --out <file>\n\n{}", api::FORMATS.join("|"), crate::query::HELP);
}

/// `--key value` pairs typed by the tool's schema; a bare argument is the path (or one of `paths`).
fn cli_args(tool: &str, rest: &[String]) -> Value {
    let params = api::TOOLS.iter().find(|t| t.0 == tool).map_or(&[][..], |t| t.2);
    let mut m = Map::new();
    let mut i = 0;
    while i < rest.len() {
        let Some(k) = rest[i].strip_prefix("--") else {
            if params.iter().any(|p| p.0 == "paths") {
                m.entry("paths").or_insert(json!([])).as_array_mut().unwrap().push(json!(rest[i]));
            } else {
                m.insert("path".into(), json!(rest[i]));
            }
            i += 1;
            continue;
        };
        let ty = params.iter().find(|p| p.0 == k).map_or("string", |p| p.1);
        let v = rest.get(i + 1).filter(|v| !v.starts_with("--") || ty == "string");
        i += 1 + v.is_some() as usize;
        m.insert(k.into(), match (ty, v) {
            ("boolean", v) => json!(v.map_or(true, |v| v != "false")),
            ("integer", Some(v)) => json!(v.parse::<u64>().unwrap_or(0)),
            ("array", Some(v)) => json!(v.split(',').map(str::trim).collect::<Vec<_>>()),
            (_, Some(v)) => json!(v),
            (_, None) => json!(true),
        });
    }
    Value::Object(m)
}

/// Minimal MCP server: newline-delimited JSON-RPC 2.0 over stdio, tools only.
fn mcp() {
    let tools: Vec<Value> = api::TOOLS
        .iter()
        .map(|(name, desc, params)| {
            let mut props = Map::new();
            for (p, ty, _, d) in *params {
                let mut schema = json!({ "type": ty, "description": d });
                if *ty == "array" {
                    schema["items"] = json!({ "type": "string" });
                }
                props.insert(p.to_string(), schema);
            }
            props.insert("format".into(), json!({ "type": "string", "enum": api::FORMATS, "description": "Output format (default json)." }));
            props.insert("out".into(), json!({ "type": "string", "description": "Write the output to this file instead of returning it." }));
            let required: Vec<&str> = params.iter().filter(|p| p.2).map(|p| p.0).collect();
            json!({ "name": name, "description": desc, "inputSchema": { "type": "object", "properties": props, "required": required } })
        })
        .collect();
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        // some shells prepend a BOM to piped input
        let Ok(req) = serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}')) else { continue };
        let Some(id) = req.get("id").cloned() else { continue }; // notifications need no reply
        let result = match req["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "diskvibe", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Disk space inspector. Start with `disks`, then `ls`/`find`/`stats` on a path. Destructive tools (`trash`, `temp clear=true`, `organize apply=true`) should be confirmed with the user first.",
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools })),
            "tools/call" => {
                let p = &req["params"];
                let res = api::run(p["name"].as_str().unwrap_or(""), p.get("arguments").unwrap_or(&json!({})), "json");
                Ok(json!({ "isError": res.is_err(), "content": [{ "type": "text", "text": res.unwrap_or_else(|e| e) }] }))
            }
            m => Err(json!({ "code": -32601, "message": format!("method not found: {m}") })),
        };
        let reply = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => gui::run(None),
        Some("mcp") => mcp(),
        Some("--help" | "-h") => help(),
        Some(t) if api::TOOLS.iter().any(|x| x.0 == t) && (t != "help" || args.len() == 1) => {
            if t == "help" {
                return help();
            }
            match api::run(t, &cli_args(t, &args[1..]), "txt") {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(p) if std::path::Path::new(p).is_dir() => gui::run(Some(p.into())),
        Some(x) => {
            eprintln!("unknown command or folder '{x}' — try: diskvibe --help");
            std::process::exit(2);
        }
    }
}
