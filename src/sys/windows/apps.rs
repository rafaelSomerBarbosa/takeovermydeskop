//! `launch_app` on Windows: opening apps the way the Start menu does. That also brings back apps
//! hidden in the system tray: single-instance apps such as WhatsApp or Discord answer a second
//! launch by showing the window they already have, which nothing else can reach while it's hidden.

use super::{program_path, with_process};
use crate::input;
use crate::window::{self, Launched, Window};
use anyhow::{Context, Result, anyhow, bail};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, PROPERTYKEY};
use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_Link_Arguments, PKEY_Link_TargetParsingPath};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, ILFree, IShellItem, IShellItem2, KF_FLAG_DEFAULT,
    SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHGetIDListFromObject,
    SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{Interface, PWSTR};

/// How long a starting app gets to show its window.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the launch gets to bring a window forward before an app that was already in front
/// counts as the one it would have shown.
const SETTLED: Duration = Duration::from_secs(1);

/// How uninstallers' names start in a few languages: they only open when asked for by name.
const UNINSTALL: [&str; 5] = ["uninstall", "desinstal", "deinstall", "désinstall", "disinstall"];

/// What tells an app's windows apart, from its entry in the Start menu's app list (Win32
/// programs and Store apps alike).
struct App {
    name: String,
    /// The shell's ID for the entry, which is the AppUserModelID for Store apps and many others.
    id: String,
    /// The program a plain shortcut starts, e.g. "code.exe". None when the shortcut passes
    /// arguments, as then the program alone (mmc.exe, a browser) doesn't tell apps apart.
    program: Option<String>,
    /// The folder that program is installed in (see `install_folder`), for apps whose windows
    /// run under a helper program, like Steam's steamwebhelper.exe.
    folder: Option<String>,
}

/// Opens the Start menu app best matching `query`, or brings it to the front if it's already
/// showing. `check_stop` runs while waiting for the window, for the emergency stop.
pub fn launch(query: &str, check_stop: &(dyn Fn() -> Result<()> + Sync)) -> Result<Launched> {
    let query = query.trim();
    if query.is_empty() {
        bail!("name the app to open, e.g. \"WhatsApp\"");
    }
    // The shell needs COM. It gets a thread of its own, so the server's main thread, which sits
    // waiting on stdin, never becomes a COM apartment that doesn't pump messages. Window handles
    // can't cross threads, so the app's window comes back as a number.
    let (note, shown) = std::thread::scope(|scope| {
        scope
            .spawn(|| unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok().context("couldn't start COM")?;
                let opened = open(query, check_stop);
                CoUninitialize();
                opened
            })
            .join()
    })
    .map_err(|_| anyhow!("opening the app crashed"))??;
    let window = match shown {
        Some(raw) => window::list()?.into_iter().find(|w| w.id.0 as isize == raw),
        None => None,
    };
    Ok(Launched::Shown(note, window))
}

/// What happened, and the app's window when it's now in front.
fn open(query: &str, check_stop: &dyn Fn() -> Result<()>) -> Result<(String, Option<isize>)> {
    let (app, item) = find(query)?;
    // Launching some apps again opens a second window, so one already on screen just comes forward.
    if let Some(window) = window::list()?.iter().find(|w| is_of(w, &app)) {
        return Ok(bring_forward(window, format!("{} was already open.", app.name)));
    }

    let before: Vec<HWND> = window::list()?.iter().map(|w| w.id).collect();
    let front_before = window::foreground()?;
    launch_item(&item).with_context(|| format!("couldn't open {}", app.name))?;
    let start = Instant::now();
    loop {
        input::pause(250);
        check_stop()?;
        let elapsed = start.elapsed();
        let timed_out = elapsed >= START_TIMEOUT;
        let windows = window::list()?;
        let front = window::foreground()?;
        let in_front = |w: &Window| Some(w.id) == front;
        let brought_up = |w: &&Window| !before.contains(&w.id) || (in_front(w) && front != front_before);
        let found = windows
            .iter()
            .find(|w| is_of(w, &app))
            // Apps whose windows run under a helper program, like Steam's steamwebhelper.exe: a
            // window named after the app or run from its folder, that the launch brought up.
            .or_else(|| windows.iter().filter(brought_up).find(|w| titled(&w.title, &app.name) || in_folder(w, &app)))
            // Such an app that was in front all along, where the launch left it.
            .or_else(|| {
                (elapsed >= SETTLED && front == front_before)
                    .then(|| windows.iter().find(|w| in_front(w) && titled(&w.title, &app.name) && in_folder(w, &app)))
                    .flatten()
            })
            // Last resort: whatever the launch brought to the front.
            .or_else(|| timed_out.then(|| windows.iter().find(|w| in_front(w) && front != front_before)).flatten());
        if let Some(window) = found {
            return Ok(bring_forward(window, format!("Opened {}.", app.name)));
        }
        if timed_out {
            let note = format!(
                "Opened {}, but no window of it showed up within {} s: it may still be starting, or it may only show a tray icon.",
                app.name,
                START_TIMEOUT.as_secs()
            );
            return Ok((note, None));
        }
    }
}

/// Focuses `window`. The app is open either way, so a refusal goes in the note rather than failing.
fn bring_forward(window: &Window, note: String) -> (String, Option<isize>) {
    match window::activate(window) {
        Ok(window) => (format!("{note} {} is in front.", window.label()), Some(window.id.0 as isize)),
        // Screenshots stay where they are: the window isn't in front to be seen.
        Err(error) => (format!("{note} {error:#}."), None),
    }
}

/// Whether `window` is surely one of the app's own: run by the program its shortcut starts or
/// one named after it, or carrying its app ID. This decides about windows that were open before
/// the launch, so titles and folders don't count here: a game run from Steam's folder isn't Steam.
fn is_of(window: &Window, app: &App) -> bool {
    let program = window.app.to_lowercase();
    !program.is_empty()
        && (app.program.as_deref() == Some(program.as_str())
            || named_after(&program, &app.name)
            || app_id(window.id).is_some_and(|id| id.eq_ignore_ascii_case(&app.id)))
}

/// Whether `window`'s program runs from the app's install folder.
fn in_folder(window: &Window, app: &App) -> bool {
    app.folder.as_ref().is_some_and(|folder| {
        program_path(window.id).is_some_and(|path| path.to_lowercase().starts_with(folder.as_str()))
    })
}

/// The AppUserModelID the taskbar groups `handle` under: set on the window itself (Store apps'
/// frames, browsers' app windows), else that of the packaged app running it. Needs COM.
fn app_id(handle: HWND) -> Option<String> {
    let from_window = unsafe { SHGetPropertyStoreForWindow::<IPropertyStore>(handle) }.ok().and_then(|store| unsafe {
        // PROPVARIANT clears itself when dropped.
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        take(PropVariantToStringAlloc(&value).ok()?)
    });
    from_window.filter(|id| !id.is_empty()).or_else(|| {
        with_process(handle, |process| {
            let mut buffer = [0u16; 256];
            let mut len = buffer.len() as u32;
            let found = unsafe { GetApplicationUserModelId(process, &mut len, Some(PWSTR(buffer.as_mut_ptr()))) };
            // The length counts the closing null.
            (found == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buffer[..(len as usize).saturating_sub(1)]))
        })
    })
}

/// The folder `program` is installed in, lowercase with a closing backslash. None for folders
/// that hold many programs (Windows, Program Files and the like), which don't tell apps apart.
fn install_folder(program: &str) -> Option<String> {
    let folder = format!("{}\\", program.rsplit_once('\\')?.0.to_lowercase());
    let known = |variable: &str| {
        std::env::var(variable).ok().map(|dir| format!("{}\\", dir.trim_end_matches('\\').to_lowercase()))
    };
    if known("SystemRoot").is_some_and(|windows| folder.starts_with(&windows)) {
        return None;
    }
    let shared = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA", "APPDATA"].map(known);
    let programs = known("LOCALAPPDATA").map(|dir| format!("{dir}programs\\"));
    if shared.iter().chain([&programs]).flatten().any(|root| *root == folder) {
        return None;
    }
    Some(folder)
}

/// Whether `program` (e.g. "discord.exe") carries the app's name: "Discord", "Docker Desktop".
fn named_after(program: &str, name: &str) -> bool {
    let stem = program.strip_suffix(".exe").unwrap_or(program).to_lowercase();
    let name = name.to_lowercase();
    stem == name || stem == name.replace(' ', "")
}

/// Whether a window title shows the app's name: "Steam", "Inbox - Outlook".
fn titled(title: &str, name: &str) -> bool {
    let (title, name) = (title.to_lowercase(), name.to_lowercase());
    title == name || [" - ", " — ", " | "].iter().any(|separator| title.ends_with(&format!("{separator}{name}")))
}

/// The Start menu app for `query`, and its entry there to launch.
fn find(query: &str) -> Result<(App, IShellItem)> {
    let (name, item) = choose(query, start_menu()?)?;
    let item2: Option<IShellItem2> = item.cast().ok();
    let property = |key: &PROPERTYKEY| item2.as_ref().and_then(|item| unsafe { item.GetString(key) }.ok().and_then(take));
    let arguments = property(&PKEY_Link_Arguments).filter(|arguments| !arguments.trim().is_empty());
    let target = property(&PKEY_Link_TargetParsingPath)
        .filter(|path| arguments.is_none() && path.to_lowercase().ends_with(".exe"));
    let program = target.as_deref().and_then(|path| path.rsplit('\\').next()).map(str::to_lowercase);
    let folder = target.as_deref().and_then(install_folder);
    let id = display_name(&item, SIGDN_PARENTRELATIVEPARSING).unwrap_or_default();
    Ok((App { name, id, program, folder }, item))
}

/// Picks the entry for `query`: the same name, else the same letters and digits, else one that
/// starts with or contains it. Partial matches only count when there's just one, so a vague name
/// doesn't start an app nobody meant. Uninstallers only count when asked for.
fn choose<T>(query: &str, entries: Vec<(String, T)>) -> Result<(String, T)> {
    let wanted = query.to_lowercase();
    let squashed = squash(query);
    let asks_uninstall = UNINSTALL.iter().any(|word| wanted.contains(word));
    let rank = |name: &str| {
        let lower = name.to_lowercase();
        if !asks_uninstall && UNINSTALL.iter().any(|word| lower.contains(word)) {
            None
        } else if lower == wanted {
            Some(0)
        } else if squash(name) == squashed {
            Some(1)
        } else if lower.starts_with(&wanted) {
            Some(2)
        } else if lower.contains(&wanted) {
            Some(3)
        } else {
            None
        }
    };
    let mut matches: Vec<(u8, String, T)> =
        entries.into_iter().filter_map(|(name, entry)| Some((rank(&name)?, name, entry))).collect();
    matches.sort_by_key(|(rank, name, _)| (*rank, name.chars().count()));
    let mut names: Vec<&str> = matches.iter().map(|(_, name, _)| name.as_str()).collect();
    names.dedup();
    match matches.first() {
        None => bail!(
            "no app called {query:?} in the Start menu. If it's running with only an icon in the system tray, \
             click that icon (the ^ arrow next to the clock shows hidden ones)"
        ),
        Some((rank, ..)) if *rank > 1 && names.len() > 1 => {
            names.truncate(10);
            bail!("several apps match {query:?}: {}. Call launch_app again with the full name.", names.join(", "))
        }
        Some(_) => {
            let (_, name, entry) = matches.swap_remove(0);
            Ok((name, entry))
        }
    }
}

/// Every app in the Start menu, as the shell's Apps folder lists them.
fn start_menu() -> Result<Vec<(String, IShellItem)>> {
    unsafe {
        let folder: IShellItem =
            SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None).context("couldn't read the Start menu")?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems).context("couldn't read the Start menu")?;
        let mut apps = Vec::new();
        loop {
            let mut batch: Vec<Option<IShellItem>> = vec![None; 64];
            let mut fetched = 0;
            items.Next(&mut batch, Some(&mut fetched)).context("couldn't read the Start menu")?;
            if fetched == 0 {
                break;
            }
            for item in batch.into_iter().flatten() {
                if let Some(name) = display_name(&item, SIGDN_NORMALDISPLAY) {
                    apps.push((name, item));
                }
            }
        }
        Ok(apps)
    }
}

/// Runs the item's default action, as clicking it in the Start menu does.
fn launch_item(item: &IShellItem) -> Result<()> {
    unsafe {
        let list = SHGetIDListFromObject(item)?;
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            // No error dialogs, which would sit there with nobody to close them; and finish
            // before returning, as this thread ends soon after.
            fMask: SEE_MASK_IDLIST | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
            lpIDList: list.cast(),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let launched = ShellExecuteExW(&mut info);
        ILFree(Some(list));
        Ok(launched?)
    }
}

fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    unsafe { item.GetDisplayName(kind) }.ok().and_then(take)
}

/// Reads and frees a string the shell allocated.
fn take(text: PWSTR) -> Option<String> {
    unsafe {
        let string = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        string
    }
}

/// Lowercase letters and digits only, so "Whats App" and "whatsapp" compare equal.
fn squash(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Rect;

    fn pick(query: &str, names: &[&str]) -> Result<String> {
        choose(query, names.iter().map(|name| (name.to_string(), ())).collect()).map(|(name, _)| name)
    }

    fn window(program: &str, title: &str) -> Window {
        let rect = Rect { left: 0, top: 0, right: 0, bottom: 0 };
        Window { id: HWND::default(), title: title.into(), app: program.into(), rect, hidden: None }
    }

    fn app(name: &str, program: Option<&str>, folder: Option<&str>) -> App {
        App { name: name.into(), id: String::new(), program: program.map(Into::into), folder: folder.map(Into::into) }
    }

    #[test]
    fn exact_name_beats_same_letters() {
        let names = ["Notepad", "Notepad++"];
        assert_eq!(pick("Notepad++", &names).unwrap(), "Notepad++");
        assert_eq!(pick("notepad", &names).unwrap(), "Notepad");
        assert_eq!(pick("whats app", &["WhatsApp"]).unwrap(), "WhatsApp");
    }

    #[test]
    fn a_single_partial_match_opens_but_several_need_the_full_name() {
        assert_eq!(pick("whats", &["WhatsApp", "Steam"]).unwrap(), "WhatsApp");
        // Letters run together across words don't count: "Bloco de notas" isn't "code".
        assert_eq!(pick("code", &["OpenCode", "Bloco de notas"]).unwrap(), "OpenCode");
        let error = pick("teams", &["Microsoft Teams", "Teams Machine-Wide Installer"]).unwrap_err().to_string();
        assert!(error.contains("Microsoft Teams") && error.contains("Teams Machine-Wide Installer"), "{error}");
    }

    #[test]
    fn uninstallers_only_when_asked() {
        let names = ["Zoom Workplace", "Uninstall Zoom Workplace", "Desinstalar Zoom"];
        assert_eq!(pick("zoom", &names).unwrap(), "Zoom Workplace");
        assert_eq!(pick("uninstall zoom workplace", &names).unwrap(), "Uninstall Zoom Workplace");
        assert!(pick("nothing like it", &names).is_err());
    }

    #[test]
    fn already_open_takes_the_apps_own_program() {
        let steam = app("Steam", Some("steam.exe"), Some(r"c:\program files (x86)\steam\"));
        assert!(is_of(&window("steam.exe", "Steam"), &steam));
        // A game run from Steam's library folder is not Steam.
        assert!(!is_of(&window("game.exe", "Some Game"), &steam));
        // Steam's own window runs under a helper: that's left to relaunching it.
        assert!(!is_of(&window("steamwebhelper.exe", "Steam"), &steam));

        let excel = app("Excel", Some("excel.exe"), Some(r"c:\program files\microsoft office\root\office16\"));
        assert!(is_of(&window("EXCEL.EXE", "Book1 - Excel"), &excel));
        assert!(!is_of(&window("WINWORD.EXE", "Document1 - Word"), &excel));

        let discord = app("Discord", None, None);
        assert!(is_of(&window("Discord.exe", "Amigos - Discord"), &discord));
        assert!(!is_of(&window("chrome.exe", "Discord - Google Chrome"), &discord));
    }

    #[test]
    fn programs_named_after_the_app() {
        assert!(named_after("discord.exe", "Discord"));
        assert!(named_after("docker desktop.exe", "Docker Desktop"));
        assert!(named_after("dockerdesktop.exe", "Docker Desktop"));
        assert!(!named_after("paintdotnet.exe", "Paint"));
        assert!(!named_after("notepad++.exe", "Notepad"));
        assert!(!named_after("steamwebhelper.exe", "Steam"));
    }

    #[test]
    fn install_folders_but_not_shared_ones() {
        assert_eq!(
            install_folder(r"C:\Program Files (x86)\Steam\steam.exe").as_deref(),
            Some(r"c:\program files (x86)\steam\")
        );
        let windows = std::env::var("SystemRoot").unwrap();
        assert_eq!(install_folder(&format!(r"{windows}\System32\mmc.exe")), None);
        let program_files = std::env::var("ProgramFiles").unwrap();
        assert_eq!(install_folder(&format!(r"{program_files}\tool.exe")), None);
    }

    #[test]
    fn titles_showing_the_app() {
        assert!(titled("Steam", "Steam"));
        assert!(titled("#general | Server - Discord", "Discord"));
        assert!(!titled("calculator - Google Search - Google Chrome", "Calculator"));
        assert!(!titled("Steam - File Explorer", "Steam"));
    }
}
