//! Lets Claude take over the Windows desktop: an MCP server that gives Claude Code eyes
//! (screenshots) and hands (mouse and keyboard), plus a launcher that hands it a task to do alone.

mod input;
mod mcp;
mod run;
mod screen;
mod tools;
mod window;

use anyhow::Result;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

const USAGE: &str = "\
takeovermydeskop: let Claude use your Windows desktop

Usage:
  takeovermydeskop serve              MCP server for Claude Code (it starts this for you)
  takeovermydeskop do <task>          Have Claude do a task on its own, e.g.
                                      takeovermydeskop do \"open Notepad and write a haiku\"
  takeovermydeskop screenshot [file]  Save what Claude sees (default screenshot.png)

Emergency stop: push the mouse pointer into a corner of the screen.";

fn main() -> Result<()> {
    // Work in physical pixels. Otherwise Windows scales screenshots and coordinates on high-DPI
    // monitors, and clicks land in the wrong place.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
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
