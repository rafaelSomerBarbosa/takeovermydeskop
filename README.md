# takeovermydeskop

Lets Claude use your Windows desktop. It's an MCP server that gives Claude Code eyes (screenshots) and hands (real mouse and keyboard input), plus a launcher that hands Claude a task to do on its own. It runs on the Claude account your `claude` CLI is logged into, so you don't need an API key.

## Get it

Download `takeovermydeskop.exe` from the [latest release](https://github.com/rafaelSomerBarbosa/takeovermydeskop/releases/latest), or build it:

```powershell
cargo build --release
```

## Use it

**Hand over a task** and watch it work:

```powershell
.\target\release\takeovermydeskop.exe do "open Notepad and write a haiku about Rust"
```

This runs `claude -p` with only the desktop tools approved. Claude prints what it says and does, and it won't stop to ask questions: it skips destructive steps the task didn't clearly ask for and explains what's left.

**Use it from any Claude Code session** by registering the server once, from the project folder:

```powershell
claude mcp add --scope user desktop -- "$PWD\target\release\takeovermydeskop.exe" serve
```

With a downloaded `takeovermydeskop.exe`, use its full path instead.

Claude Code asks before each desktop action. To skip that, allow `mcp__desktop` in your permissions.

**See what Claude sees:** `takeovermydeskop screenshot shot.png`.

## Emergency stop

Push the mouse pointer into any corner of a screen. The next action Claude tries is refused with a "stopped by user" message, and Claude ends the task. In an interactive session you can also press Esc in Claude Code.

## Tools

| Tool | What it does |
| --- | --- |
| `screenshot` | Current monitor, mouse pointer included. `monitor` switches monitors. On ultrawide monitors, `part` (`left`/`center`/`right`) shows a sharper 16:9 slice. |
| `zoom` | Magnified view of a rectangle, for reading small text |
| `click`, `move_mouse`, `drag`, `scroll` | Mouse input in screenshot coordinates (`click` takes `clicks`, `button` and `modifiers`) |
| `type_text` | Types any text, including accents and emoji (`\n` presses Enter) |
| `press_keys` | Shortcuts like `ctrl+s`, `alt+tab`, `win+r`, with `repeat` |
| `wait` | Waits, then takes a screenshot |
| `list_windows`, `focus_window` | Open windows, and bringing one to the front by title or app name |

Action tools return a screenshot afterwards unless they're called with `screenshot: false`.

## Notes

- Screenshots get about 1080p's worth of pixels and are at most 2000 px wide. Bigger images get shrunk on the way to the model, which would throw clicks off. Set `TAKEOVER_MAX_PIXELS` to trade accuracy against usage.
- Windows blocks this input in UAC prompts, on the lock screen and in apps running as administrator.
- `TAKEOVER_CLAUDE_BIN` points `do` at a different `claude` executable.

## License

[MIT](LICENSE)
