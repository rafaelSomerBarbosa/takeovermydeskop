# takeovermydeskop

Lets Claude use your desktop on Windows and Linux. It's an MCP server that gives Claude Code eyes (screenshots) and hands (real mouse and keyboard input), plus a launcher that hands Claude a task to do on its own. It runs on the Claude account your `claude` CLI is logged into, so you don't need an API key.

## Where it runs

| Desktop | Status |
| --- | --- |
| Windows | Works |
| Hyprland | Works, with no root access or helper programs |
| Linux in an X11 session (Xfce, Cinnamon, MATE, i3 and others) | Works |
| GNOME, KDE Plasma, Sway and other Wayland desktops | Not yet: it stops with an error saying so. If your login screen offers an X11 session, use that |

## Get it

Download it from the [latest release](https://github.com/rafaelSomerBarbosa/takeovermydeskop/releases/latest): `takeovermydeskop.exe` for Windows, or `takeovermydeskop` for Linux, which is a static x86-64 binary that runs on any distribution (`chmod +x` it first). Or build it:

```sh
cargo build --release
```

## Use it

The examples use the Linux path; on Windows it's `.\target\release\takeovermydeskop.exe`.

**Hand over a task** and watch it work:

```sh
./target/release/takeovermydeskop do "open a text editor and write a haiku about Rust"
```

This runs `claude -p` with the desktop tools as the only tools Claude has, whatever your Claude Code permissions allow: it can't run commands or read files behind your back. That's not a sandbox, though. With the keyboard it can do anything you could, including typing into a terminal, but all of it happens on the screen, where you can see it and stop it. Claude prints what it says and does, and it won't stop to ask questions: it skips destructive steps the task didn't clearly ask for and explains what's left.

**Use it from any Claude Code session** by registering the server once, from the project folder:

```sh
claude mcp add --scope user desktop -- "$PWD/target/release/takeovermydeskop" serve
```

With a downloaded binary, use its full path instead.

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
| `press_keys` | Shortcuts like `ctrl+s`, `alt+tab`, `win+r` (`win` is the Super key on Linux), with `repeat` |
| `wait` | Waits, then takes a screenshot |
| `list_windows`, `focus_window` | Open windows, and bringing one to the front by title or app name |
| `launch_app` | Linux only: opens an installed app by name or kind (`firefox`, `text editor`), as an applications menu would. It takes no command line, and only sees apps a menu would list |

Action tools return a screenshot afterwards unless they're called with `screenshot: false`.

## Notes

- Screenshots get about 1080p's worth of pixels and are at most 2000 px wide. Bigger images get shrunk on the way to the model, which would throw clicks off. Set `TAKEOVER_MAX_PIXELS` to trade accuracy against usage.
- Windows blocks this input in UAC prompts, on the lock screen and in apps running as administrator.
- On Linux, run it from inside your desktop session: it finds the desktop through variables like `WAYLAND_DISPLAY` and `DISPLAY`.
- On Hyprland, Claude types through a virtual keyboard with a US layout of its own, which leaves your keyboard's layout alone. It can't toggle Caps Lock or Num Lock, and doesn't need to: `type_text` types either case.
- `TAKEOVER_CLAUDE_BIN` points `do` at a different `claude` executable.

## License

[MIT](LICENSE)
