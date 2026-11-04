//! Lets Claude take over the desktop on Windows and Linux: an MCP server that gives Claude Code
//! eyes (screenshots) and hands (mouse and keyboard), plus a launcher that hands it a task to do
//! alone. Everything that differs between the platforms lives in `sys`.

mod input;
mod mcp;
mod run;
mod screen;
#[cfg_attr(windows, path = "sys/windows.rs")]
#[cfg_attr(target_os = "linux", path = "sys/linux/mod.rs")]
mod sys;
mod tools;
mod window;

use anyhow::Result;

const USAGE: &str = "\
takeovermydeskop: let Claude use your desktop

Usage:
  takeovermydeskop serve              MCP server for Claude Code (it starts this for you)
  takeovermydeskop do <task>          Have Claude do a task on its own, e.g.
                                      takeovermydeskop do \"open a text editor and write a haiku\"
  takeovermydeskop screenshot [file]  Save what Claude sees (default screenshot.png)

Emergency stop: push the mouse pointer into a corner of the screen.";

fn main() -> Result<()> {
    sys::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => mcp::serve(),
        Some("do") if args.len() > 1 => run::run(&args[1..].join(" ")),
        Some("screenshot") => {
            let path = args.get(1).map_or("screenshot.png", String::as_str);
            tools::Desktop::new().save_screenshot(path)?;
            println!("Saved {path}");
            Ok(())
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}
