//! `takeovermydeskop do <task>`: hands a task to Claude Code in headless mode (`claude -p`), with
//! this program as its only MCP server, and prints what Claude says and does until it's done.
//! It runs on whatever Claude account the CLI is logged into, like an interactive session.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const UNATTENDED: &str = "You're running unattended from the takeovermydeskop launcher: nobody is watching \
and nobody can answer questions until you finish. Carry the task through with the desktop tools. If a step \
would be destructive or hard to undo and the task doesn't clearly ask for it (deleting, sending, buying, \
changing settings), don't do it: finish what you safely can and explain what's left. End with a short \
summary of what you did.";

pub fn run(task: &str) -> Result<()> {
    let exe = std::env::current_exe().context("couldn't find this program's own path")?;
    let config = json!({ "mcpServers": { "desktop": { "type": "stdio", "command": exe, "args": ["serve"] } } });
    let config_path = std::env::temp_dir().join("takeovermydeskop-mcp.json");
    std::fs::write(&config_path, config.to_string())?;

    let bin = std::env::var("TAKEOVER_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let mut child = Command::new(&bin)
        .args(["-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config"])
        .arg("--mcp-config")
        .arg(&config_path)
        // Approves the desktop tools without prompting; nobody is there to answer a prompt.
        .args(["--allowedTools", "mcp__desktop"])
        .args(["--append-system-prompt", UNATTENDED])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("couldn't run `{bin}`; install Claude Code or set TAKEOVER_CLAUDE_BIN"))?;

    // The task goes on stdin: --allowedTools takes a list and would swallow it as an argument.
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(task.as_bytes())?;
    drop(stdin);

    let mut finished = false;
    for line in BufReader::new(child.stdout.take().context("no stdout")?).lines() {
        let Ok(event) = serde_json::from_str::<Value>(&line?) else { continue };
        match event["type"].as_str().unwrap_or_default() {
            "system" if event["subtype"] == "init" => {
                let servers = event["mcp_servers"].as_array().map(Vec::as_slice).unwrap_or_default();
                if !servers.iter().any(|s| s["name"] == "desktop" && s["status"] == "connected") {
                    eprintln!("warning: the desktop tools didn't connect: {}", event["mcp_servers"]);
                }
            }
            "assistant" => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => println!("{}", block["text"].as_str().unwrap_or_default().trim()),
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or_default();
                            println!("  > {} {}", name.trim_start_matches("mcp__desktop__"), shorten(&block["input"].to_string(), 160));
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" && block["is_error"] == true {
                        println!("  ! {}", shorten(&result_text(&block["content"]), 300));
                    }
                }
            }
            "result" => {
                finished = true;
                let outcome = if event["is_error"] == true { "Failed" } else { "Done" };
                let seconds = event["duration_ms"].as_f64().unwrap_or_default() / 1000.0;
                println!("\n{outcome} after {} turns in {seconds:.0} s.", event["num_turns"]);
            }
            _ => {}
        }
    }
    let status = child.wait()?;
    if !finished || !status.success() {
        bail!("claude exited with {status}");
    }
    Ok(())
}

/// A tool result's content is either a string or a list of blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(" "),
        other => other.to_string(),
    }
}

fn shorten(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}
