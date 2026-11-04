//! Starting installed apps. A tiling desktop has no Start menu for Claude to click, so `launch_app`
//! offers what an applications menu would: the desktop entries of the freedesktop.org spec, by name.
//!
//! It takes a name and no command line on purpose: anything that could pass arguments could run
//! `sh -c`, which would hand back the shell that `do` keeps from Claude.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, PartialEq)]
struct App {
    /// The entry's file name without `.desktop`, e.g. "org.kde.dolphin".
    id: String,
    name: String,
    /// What kind of app it is, e.g. "Web Browser".
    generic_name: String,
    command: Vec<String>,
    /// The directory to run it in, for the few apps that say.
    directory: Option<String>,
    /// Whether it needs a terminal to run in.
    terminal: bool,
}

impl App {
    /// The last part of a reverse-DNS id: "dolphin" for "org.kde.dolphin".
    fn short_id(&self) -> &str {
        self.id.rsplit('.').next().unwrap_or(&self.id)
    }
}

/// Reads a desktop entry. Only apps a menu would show count: most entries on a real system are
/// handlers marked `NoDisplay` that aren't meant to be started by hand.
fn parse_entry(id: &str, text: &str) -> Option<App> {
    let (mut name, mut generic_name, mut exec, mut directory) = (None, String::new(), None, None);
    let (mut application, mut terminal, mut in_entry) = (false, false, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            // Other groups, like [Desktop Action new-window], reuse the same keys.
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_entry) else { continue };
        let value = value.trim();
        match key.trim() {
            "Type" => application = value == "Application",
            "Name" => name = Some(value.to_string()),
            "GenericName" => generic_name = value.to_string(),
            "Exec" => exec = Some(parse_exec(value)),
            "Path" if !value.is_empty() => directory = Some(value.to_string()),
            "Terminal" => terminal = value == "true",
            "Hidden" | "NoDisplay" if value == "true" => return None,
            _ => {}
        }
    }
    let command = exec.filter(|command| !command.is_empty())?;
    application.then_some(App { id: id.to_string(), name: name?, generic_name, command, directory, terminal })
}

/// Splits an `Exec` value into arguments, leaving out field codes like `%u`: those stand for files
/// to open, and we open none. The value is escaped twice over: once as a string in the file, once
/// as a command line.
fn parse_exec(exec: &str) -> Vec<String> {
    let exec = exec.replace("\\\\", "\\");
    let (mut args, mut current, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => current.extend(chars.next()),
            ' ' | '\t' if !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            '%' => {
                if chars.next() == Some('%') {
                    current.push('%');
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Where desktop entries live, the ones that take precedence first.
fn application_dirs() -> Vec<PathBuf> {
    let set = |name: &str| env::var(name).ok().filter(|value| !value.is_empty());
    let home = set("XDG_DATA_HOME").or_else(|| set("HOME").map(|home| format!("{home}/.local/share")));
    let system = set("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.iter().map(String::as_str).chain(system.split(':')).map(|dir| PathBuf::from(dir).join("applications")).collect()
}

/// Installed apps, those that open a window of their own first, then by name.
fn installed() -> Vec<App> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for entry in application_dirs().iter().filter_map(|dir| fs::read_dir(dir).ok()).flatten().flatten() {
        let path = entry.path();
        let Some(id) = path.file_name().and_then(|name| name.to_str()?.strip_suffix(".desktop")) else { continue };
        // An entry hides the ones of the same id further down, even if it hides itself too: that's
        // how a user takes an app off the menu.
        if seen.insert(id.to_string())
            && let Ok(text) = fs::read_to_string(&path)
            && let Some(app) = parse_entry(id, &text)
        {
            apps.push(app);
        }
    }
    apps.sort_by_key(|app| (app.terminal, app.name.to_lowercase()));
    apps
}

/// The best match for `query`, from what it could only mean to what it might. Exact kinds beat
/// partial names, so "web browser" finds a browser and not a "Web Browser Settings".
fn find<'a>(apps: &'a [App], query: &str) -> Option<&'a App> {
    let query = query.trim().to_lowercase();
    let name = |app: &&App| app.name.to_lowercase();
    let generic_name = |app: &&App| app.generic_name.to_lowercase();
    apps.iter()
        .find(|app| name(app) == query)
        .or_else(|| apps.iter().find(|app| app.id.to_lowercase() == query || app.short_id().to_lowercase() == query))
        .or_else(|| apps.iter().find(|app| generic_name(app) == query))
        .or_else(|| apps.iter().find(|app| name(app).contains(&query)))
        .or_else(|| apps.iter().find(|app| generic_name(app).contains(&query)))
}

/// Starts the best match for `query` and returns its name.
pub fn launch(query: &str) -> Result<String> {
    if query.trim().is_empty() {
        bail!("say which app to open");
    }
    let apps = installed();
    let Some(app) = find(&apps, query) else {
        let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
        bail!("no installed app matches {query:?}; the installed apps are: {}", names.join(", "));
    };
    if app.terminal {
        bail!("{} runs inside a terminal: open a terminal and start it there", app.name);
    }
    let (program, args) = app.command.split_first().context("the app has no command")?;
    let mut command = Command::new(program);
    // Its own process group keeps it running after we exit, and it must stay off our stdout, which
    // carries the MCP protocol.
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    if let Some(directory) = &app.directory {
        command.current_dir(directory);
    }
    let mut child = command.spawn().with_context(|| format!("couldn't start {}", app.name))?;
    // Collect it when it exits, or it stays a zombie for as long as we run.
    std::thread::spawn(move || child.wait());
    Ok(app.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, generic_name: &str, terminal: bool) -> App {
        App {
            id: name.to_lowercase(),
            name: name.into(),
            generic_name: generic_name.into(),
            command: vec![name.into()],
            directory: None,
            terminal,
        }
    }

    #[test]
    fn splits_a_command_line() {
        assert_eq!(parse_exec("firefox %u"), ["firefox"]);
        assert_eq!(parse_exec("libreoffice --math %U"), ["libreoffice", "--math"]);
        assert_eq!(parse_exec("  a   b  "), ["a", "b"]);
    }

    #[test]
    fn keeps_quoted_spaces_together() {
        assert_eq!(parse_exec(r#""/opt/my app/run" --flag"#), ["/opt/my app/run", "--flag"]);
    }

    #[test]
    fn undoes_both_levels_of_escaping() {
        // In the file: a quoted argument holding a literal double quote.
        assert_eq!(parse_exec(r#"sh -c "echo \\"hi\\"""#), ["sh", "-c", r#"echo "hi""#]);
    }

    #[test]
    fn leaves_out_field_codes_but_keeps_literal_percents() {
        assert_eq!(parse_exec("app --url=%u %F --rate=100%%"), ["app", "--url=", "--rate=100%"]);
    }

    #[test]
    fn reads_an_entry_and_ignores_its_other_groups() {
        let text = "[Desktop Entry]\nType=Application\nName=Firefox\nName[pt_BR]=Navegador\nGenericName=Web Browser\nExec=firefox %u\n\n[Desktop Action new-window]\nName=New Window\nExec=firefox --new-window %u\n";
        let app = parse_entry("org.mozilla.firefox", text).unwrap();
        assert_eq!(app.name, "Firefox");
        assert_eq!(app.generic_name, "Web Browser");
        assert_eq!(app.command, ["firefox"]);
        assert_eq!(app.short_id(), "firefox");
    }

    #[test]
    fn skips_what_a_menu_would_not_show() {
        let entry = |extra: &str| format!("[Desktop Entry]\nType=Application\nName=A\nExec=a\n{extra}\n");
        assert!(parse_entry("a", &entry("")).is_some());
        assert!(parse_entry("a", &entry("NoDisplay=true")).is_none());
        assert!(parse_entry("a", &entry("Hidden=true")).is_none());
        assert!(parse_entry("a", "[Desktop Entry]\nType=Link\nName=A\nExec=a\n").is_none());
        assert!(parse_entry("a", "[Desktop Entry]\nType=Application\nName=A\n").is_none());
        assert!(parse_entry("a", "[Desktop Entry]\nType=Application\nExec=a\n").is_none());
    }

    #[test]
    fn sees_settings_in_any_order() {
        let text = "[Desktop Entry]\nExec=a\nName=A\nTerminal=true\nPath=/srv\nType=Application\n";
        let app = parse_entry("a", text).unwrap();
        assert!(app.terminal);
        assert_eq!(app.directory.as_deref(), Some("/srv"));
    }

    #[test]
    fn prefers_exact_matches_to_partial_ones() {
        let apps = [app("Journald Browser", "Journald Browser", false), app("Firefox", "Web Browser", false)];
        assert_eq!(find(&apps, "FIREFOX").unwrap().name, "Firefox");
        assert_eq!(find(&apps, "web browser").unwrap().name, "Firefox");
        assert_eq!(find(&apps, "fire").unwrap().name, "Firefox");
        assert_eq!(find(&apps, "journald").unwrap().name, "Journald Browser");
        assert!(find(&apps, "calculator").is_none());
    }

    #[test]
    fn finds_an_app_by_the_end_of_its_id() {
        let apps = [App { id: "org.kde.dolphin".into(), ..app("File Manager", "", false) }];
        assert_eq!(find(&apps, "dolphin").unwrap().name, "File Manager");
    }

    #[test]
    fn a_kind_finds_a_windowed_app_before_a_terminal_one() {
        // In the order `installed` sorts them.
        let apps = [app("KWrite", "Text Editor", false), app("Neovim", "Text Editor", true)];
        assert_eq!(find(&apps, "text editor").unwrap().name, "KWrite");
    }

    #[test]
    fn a_command_line_is_not_a_name() {
        let apps = [app("kitty", "Terminal emulator", false)];
        assert!(find(&apps, "kitty -e sh").is_none());
        assert!(find(&apps, "kitty; rm -rf ~").is_none());
    }
}
