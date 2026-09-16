//! wsltray: a tray indicator for WSL2 (on/off, CPU and memory share of the
//! host, start/restart/shutdown from the menu). Pure Win32 through
//! `windows-sys`.
//!
//! # How the program is put together
//!
//! ```text
//!  main()            parse flags, load config, single-instance mutex, create App
//!    |
//!    +-- create_window()      hidden top-level window; owns the tray icon,
//!    |                        receives its callbacks and the poll timer
//!    +-- add_tray_icon()      Shell_NotifyIconW(NIM_ADD), version 4
//!    +-- sync_app_autostart() writes/deletes the Run key per autostart
//!    +-- start() if autoboot  boots WSL2 in the background
//!    +-- SetTimer(-poll)      WM_TIMER every 5 s by default
//!    +-- message loop         GetMessageW / DispatchMessageW until WM_QUIT
//!
//!  wnd_proc -> App::handle
//!    WM_TIMER          -> tick(): Monitor::poll(), redraw icon/tooltip if changed
//!    WM_TRAY_CALLBACK  -> show_menu() on click / keyboard select / context menu
//!    WM_REFRESH_NOW    -> tick(true), posted by the Start/Restart/Shutdown
//!                         thread when its `wsl.exe` command(s) are done
//!    TaskbarCreated    -> add_tray_icon() again after an Explorer restart
//!    WM_DESTROY        -> remove the icon, PostQuitMessage (WSL2 itself is
//!                         left running; see below)
//! ```
//!
//! The modules split as follows:
//!
//! * [`monitor`] finds the WSL2 VM process, computes CPU / memory numbers, and
//!   runs the `wsl.exe` commands (start/shutdown). It knows nothing about the
//!   UI.
//! * [`config`] loads the optional `wsltray.ini` settings file.
//! * [`icon`] loads one of the two embedded `.ico` files (colour / mono) as
//!   an `HICON`, picked by the `icon` config key and whether WSL2 is running.
//! * this file: command line, window, tray icon, menu, registry (the
//!   `autostart` Run key and the Windows 11 "show next to the clock" flag),
//!   diagnostics log.
//!
//! # WSL2 lifecycle
//!
//! This app does not keep any process of its own running to hold WSL2 up:
//! once the VM boots, `.wslconfig`'s own `vmIdleTimeout` (see the README) is
//! what keeps it from idling out, if the user has set that. **Start** just
//! runs `wsl.exe [--distribution <name>] -- exit` ([`monitor::start_wsl`]) to
//! boot the VM (and that distribution) and waits for the trivial `exit`
//! command to return, exactly as typing `wsl -- exit` into Win+R would; the
//! process does not outlive the command. **Shutdown** runs `wsl --shutdown`
//! (confirmed with a dialog, since it affects every distribution) and
//! **Restart** is Shutdown followed by Start. All three run on a helper
//! thread and report back with `WM_REFRESH_NOW`; [`App::busy`] tracks which
//! one is in flight (`Starting` / `Restarting` / `ShuttingDown`), shown as
//! the tooltip status line and used to grey out the other menu commands
//! meanwhile. Exit does not shut WSL2 down: it is expected to keep running
//! (or not) exactly as `.wslconfig` and the user's own use of `wsl.exe`
//! elsewhere dictate.
//!
//! # Threading
//!
//! Everything runs on the main thread, which is also the UI thread. The only
//! exceptions are the helper threads spawned by Start/Restart/Shutdown to run
//! the blocking `wsl.exe` commands; they talk back only through
//! `PostMessageW`, which is thread-safe.
//!
//! # Re-entrancy
//!
//! `TrackPopupMenuEx`, `MessageBoxW` and even `Shell_NotifyIconW` run a nested
//! message loop, so `wnd_proc` can be entered again while one of them is on
//! the stack. Application state therefore lives in [`App`] behind `Cell` /
//! `RefCell`, and no `RefCell` borrow is ever held across a call that can pump
//! messages. [`App::menu_open`] additionally stops a second menu from opening
//! while the first one, or a dialog it launched, is still up.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod config;
mod icon;
mod monitor;

use std::cell::{Cell, OnceCell, RefCell};
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, SYSTEMTIME, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_SZ,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, KillTimer,
    LoadCursorW, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenuEx, TranslateMessage,
    CW_USEDEFAULT, HICON, IDC_ARROW, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION,
    MB_ICONQUESTION, MB_YESNO, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_TIMER, WNDCLASSEXW,
};

use config::Config;
use icon::Kind as IconKind;
use monitor::{format_bytes, quote_arg, shutdown_wsl, start_wsl, Monitor, Status};

/// Private message the shell sends to our window for tray-icon events
/// (`NOTIFYICONDATAW::uCallbackMessage`). With `NOTIFYICON_VERSION_4` the
/// event id is in the low word of `lParam`.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
/// Keyboard activation of the icon (Enter/Space). `windows-sys` exports
/// `NIN_SELECT` and `NINF_KEY` but not their combination.
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
/// Posted by the Start/Restart/Shutdown thread once its `wsl.exe` command(s)
/// have returned, so the UI thread re-polls immediately instead of waiting
/// for the next timer tick.
const WM_REFRESH_NOW: u32 = WM_APP + 2;

/// `SetTimer` id of the poll timer.
const TIMER_POLL: usize = 1;

// Menu command ids returned by TrackPopupMenuEx(TPM_RETURNCMD).
const IDM_START: usize = 1;
const IDM_RESTART: usize = 2;
const IDM_SHUTDOWN: usize = 3;
const IDM_EXPLORER: usize = 4;
const IDM_TERMINAL: usize = 5;
const IDM_EXIT: usize = 6;

/// Window class of the hidden window. Also handy for finding the window from
/// outside (`FindWindowW`) when automating tests.
const CLASS_NAME: &str = "WSLTrayWindow";
/// Caption used for message boxes.
const APP_TITLE: &str = "WSL2 Tray";

/// Per-user autostart key and the value name written there by
/// "Start with Windows".
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WSLTray";

/// Encodes a string as NUL-terminated UTF-16 for the `*W` Win32 functions.
///
/// The returned `Vec` must outlive the call it is passed to; for structures
/// that keep the pointer (e.g. `WNDCLASSEXW::lpszClassName`) bind it to a
/// local first.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- command line ----

/// Parsed command line. Defaults are documented in [`usage`].
struct Options {
    /// Interval of the presence check (`-poll`).
    poll: Duration,
    /// Interval of the CPU/memory refresh while the VM runs (`-interval`).
    stats: Duration,
    /// Image name of the VM process (`-process`), matched case-insensitively.
    process: String,
    /// Diagnostics log file (`-log`), appended to.
    log: Option<String>,
    /// Config file (`-config`); defaults to `wsltray.ini` next to the exe.
    config: Option<String>,
}

/// Default image name of the WSL2 VM process. Windows 11 calls it `vmmemWSL`;
/// the `win10` Cargo feature builds the Windows 10 variant, where it is
/// `vmmem`. Either build can be pointed at the other name with `-process`.
const DEFAULT_PROCESS: &str = if cfg!(feature = "win10") {
    "vmmem"
} else {
    "vmmemWSL"
};

/// Help text for `-h` and flag errors. A function rather than a constant only
/// because the `--process` default is chosen per build.
fn usage() -> String {
    format!(
        "wsltray [--poll 5s] [--interval 30s] [--process {DEFAULT_PROCESS}] [--log FILE] [--config FILE]

  --poll         how often to check whether WSL2 is running (cheap)
  --interval     how often to refresh CPU/memory while WSL2 is running
  --process      name of the WSL2 VM process
  --log          append diagnostic log lines to this file
  --config       config file to read (default: wsltray.ini next to the exe)"
    )
}

/// Go `time.ParseDuration` grammar: "0", or a sequence of `<number><unit>`
/// terms such as "1m30s" or "1.5h" with units ns/us/µs/ms/s/m/h. A unit-less
/// number other than 0 is an error. Negative values (which Go accepts and then
/// feeds into SetTimer, where they only misconfigure the poll timer) are rejected.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.strip_prefix('+').unwrap_or(s);
    if s == "0" {
        return Some(Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let mut ns = 0.0f64;
    let mut rest = s;
    while !rest.is_empty() {
        let i = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (num, tail) = rest.split_at(i);
        let j = tail
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(tail.len());
        let (unit, next) = tail.split_at(j);
        let n: f64 = num.parse().ok()?; // "" and "." fail; "1." and ".5" are fine, as in Go
        let scale = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        ns += n * scale;
        rest = next;
    }
    (ns <= i64::MAX as f64).then(|| Duration::from_nanos(ns.round() as u64))
}

/// Parses the command line the way Go's `flag` package does: `-name value`,
/// `-name=value` and `--name...` are all accepted; `--` or the first argument
/// not starting with `-` ends flag parsing and the rest is ignored.
/// `Ok(None)` means `-h`/`-help` was given.
fn parse_args() -> Result<Option<Options>, String> {
    let mut o = Options {
        poll: Duration::from_secs(5),
        stats: Duration::from_secs(30),
        process: DEFAULT_PROCESS.into(),
        log: None,
        config: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--" || a.len() < 2 || !a.starts_with('-') {
            break;
        }
        let name = a.strip_prefix("--").unwrap_or(&a[1..]);
        if name.starts_with(['-', '=']) {
            return Err(format!("bad flag syntax: {a}\n\n{}", usage()));
        }
        let (key, mut inline) = match name.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (name, None),
        };
        let mut value = || {
            inline
                .take()
                .or_else(|| it.next())
                .ok_or_else(|| format!("missing value for --{key}\n\n{}", usage()))
        };
        match key {
            "poll" => o.poll = parse_duration(&value()?).ok_or("bad --poll duration")?,
            "interval" => o.stats = parse_duration(&value()?).ok_or("bad --interval duration")?,
            "process" => o.process = value()?,
            "log" => o.log = Some(value()?),
            "config" => o.config = Some(value()?),
            "h" | "help" => return Ok(None),
            _ => return Err(format!("unknown flag: {a}\n\n{}", usage())),
        }
    }
    Ok(Some(o))
}

// ---- diagnostics ----

/// The `-log` file, if one was opened. Shared with the shutdown thread.
static LOG: Mutex<Option<File>> = Mutex::new(None);
/// Cheap pre-check so the poll path does no formatting when logging is off.
static LOG_ON: AtomicBool = AtomicBool::new(false);

/// Appends a line to the `-log` file. Without one, the arguments are not
/// even formatted. Same line format as the Go version's `log` package.
macro_rules! log {
    ($($a:tt)*) => {
        if LOG_ON.load(Ordering::Relaxed) {
            log_write(format_args!($($a)*));
        }
    };
}

fn log_write(args: std::fmt::Arguments) {
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let t = local_time();
            // Same prefix as Go's default logger (LstdFlags).
            let _ = writeln!(
                f,
                "{:04}/{:02}/{:02} {:02}:{:02}:{:02} {args}",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
            );
        }
    }
}

/// Local wall-clock time via `GetLocalTime` (no chrono dependency needed).
fn local_time() -> SYSTEMTIME {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    t
}

/// `MessageBoxW` with the application title. Returns the button id (`IDYES`
/// etc.). Blocks and pumps messages until dismissed, see the re-entrancy note
/// in the module docs.
fn message_box(hwnd: HWND, text: &str, flags: u32) -> i32 {
    message_box_titled(hwnd, APP_TITLE, text, flags)
}

/// `MessageBoxW` with a caller-chosen title, for dialogs (such as the
/// Shutdown confirmation) that name the specific action rather than the app.
fn message_box_titled(hwnd: HWND, title: &str, text: &str, flags: u32) -> i32 {
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide(title).as_ptr(), flags) }
}

/// Hands back to Windows any physical pages the process is not actively
/// using. [`Monitor`]'s process-snapshot buffer grows to fit the largest
/// process count seen and is never shrunk back down, so without this the
/// reported memory footprint would keep reflecting that one-off peak (e.g.
/// after opening Explorer or a terminal) rather than the much smaller
/// working set the app actually needs between polls.
fn trim_working_set() {
    unsafe { SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0) };
}

// ---- application state ----

/// What Start/Restart/Shutdown is doing right now, if anything. Shown as the
/// tooltip status line, and used to grey out the menu commands while one is
/// in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// No command in flight; the status line follows [`Status::running`].
    Idle,
    /// `Start`'s `wsl.exe ... -- exit` is running on a helper thread; cleared
    /// when it posts `WM_REFRESH_NOW`.
    Starting,
    /// `Restart`'s shutdown-then-start is running on a helper thread;
    /// cleared when it posts `WM_REFRESH_NOW`.
    Restarting,
    /// `Shutdown`'s `wsl --shutdown` is running on a helper thread; cleared
    /// when it posts `WM_REFRESH_NOW`.
    ShuttingDown,
}

/// All mutable state of the program. There is exactly one instance, stored in
/// the [`APP`] thread-local of the UI thread and reached through
/// [`with_app`] from the window procedure.
///
/// Every field is a `Cell` or `RefCell` because the window procedure is
/// re-entered while `TrackPopupMenuEx` / `MessageBoxW` / `Shell_NotifyIconW`
/// pump messages; see the module docs. No `RefCell` borrow is held across
/// such a call.
struct App {
    /// The hidden window that owns the tray icon and the popup menu.
    hwnd: Cell<HWND>,
    /// The `NOTIFYICONDATAW` last passed to the shell. Kept so `NIM_MODIFY`
    /// and `NIM_DELETE` can reuse the same identity (`hWnd` + `uID`).
    nid: RefCell<NOTIFYICONDATAW>,
    /// Current tray icon. Replaced (and the old one destroyed) only when the
    /// colour level changes.
    hicon: Cell<HICON>,
    /// The WSL2 sampler.
    mon: RefCell<Monitor>,
    /// Icon edge length in pixels: `SM_CXSMICON` at the current DPI
    /// (16 at 100 %, 20 at 125 %, 24 at 150 %, ...).
    icon_size: usize,
    /// What Start/Restart/Shutdown is doing right now; see [`Busy`]. Blocks a
    /// second one of these from starting while one is already in flight.
    busy: Cell<Busy>,
    /// Re-entrancy guard for [`App::show_menu`]; held until the chosen
    /// command (including any dialog it shows) has finished.
    menu_open: Cell<bool>,
    /// Id of the registered `"TaskbarCreated"` message, broadcast by a new
    /// Explorer instance; the icon has to be added again then.
    taskbar_created: Cell<u32>,
    /// Which of the two embedded icons `hicon` currently shows, to skip
    /// redundant reloads.
    last_icon: Cell<Option<IconKind>>,
    /// Windows 11 promotion (see [`promote_tray_icon`]) is done, or not
    /// applicable on this Windows version.
    promoted: Cell<bool>,
    /// Number of promotion attempts; Explorer creates the registry entry a
    /// little after `NIM_ADD`, so the first attempts may find nothing.
    promote_tries: Cell<u32>,
    /// Canonical executable path in Win32 syntax, used to find our entry
    /// under `NotifyIconSettings` (Explorer stores the resolved path there).
    exe_path: String,
    /// Arguments this instance was started with, replayed into the autostart
    /// Run value so an autostarted copy behaves the same.
    launch_args: Vec<String>,
    /// Settings loaded from the config file; see [`config`].
    config: Config,
}

thread_local! {
    /// The single [`App`], owned by the UI thread. `OnceCell` rather than
    /// `RefCell<Option<App>>` so a re-entered `wnd_proc` never hits a borrow
    /// panic just for looking the state up.
    static APP: OnceCell<App> = const { OnceCell::new() };
}

/// Runs `f` with the [`App`] if it has been created (it always has by the
/// time any window message arrives, but `wnd_proc` cannot assume that).
fn with_app<R>(f: impl FnOnce(&App) -> R) -> Option<R> {
    APP.with(|a| a.get().map(f))
}

/// Entry point: parses flags, handles the single-instance early exit,
/// creates the window and icon, then runs the message loop until `WM_QUIT`.
fn main() {
    let opts = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            message_box(null_mut(), &usage(), MB_ICONINFORMATION);
            return;
        }
        Err(e) => {
            message_box(null_mut(), &e, MB_ICONERROR);
            std::process::exit(2);
        }
    };

    if let Some(path) = &opts.log {
        if let Ok(f) = File::options().create(true).append(true).open(path) {
            *LOG.lock().unwrap() = Some(f);
            LOG_ON.store(true, Ordering::Relaxed);
        }
    }

    let config_path = opts
        .config
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(config::default_path);
    let config = Config::load(&config_path);

    // Single instance per session: a named mutex in the Local\ namespace. If
    // it already exists another copy is running and this one exits quietly.
    // The handle is intentionally leaked; the OS releases it with the process.
    unsafe {
        CreateMutexW(null(), 0, wide(r"Local\WSLTray.SingleInstance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
    }

    // DPI awareness comes from the embedded manifest (per-monitor v2), so
    // SM_CXSMICON already reflects the taskbar's scale.
    let icon_size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as usize;

    // Explorer records the icon's ExecutablePath in resolved form (junctions,
    // subst drives and on-disk case), so the promotion match uses the
    // canonical path mapped back to Win32 syntax. The Run value written by
    // "Start with Windows" uses the launch path as-is (see set_autostart).
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok().or(Some(p)))
        .map(|p| win32_path(&p.to_string_lossy()))
        .unwrap_or_default();

    let app = App {
        hwnd: Cell::new(null_mut()),
        nid: RefCell::new(unsafe { std::mem::zeroed() }),
        hicon: Cell::new(null_mut()),
        mon: RefCell::new(Monitor::new(&opts.process, opts.stats)),
        icon_size,
        busy: Cell::new(Busy::Idle),
        menu_open: Cell::new(false),
        taskbar_created: Cell::new(0),
        last_icon: Cell::new(None),
        promoted: Cell::new(false),
        promote_tries: Cell::new(0),
        exe_path,
        launch_args: std::env::args().skip(1).collect(),
        config,
    };
    APP.with(|slot| {
        if slot.set(app).is_err() {
            unreachable!("app initialised twice");
        }
    });

    if let Err(e) = with_app(|a| a.create_window()).unwrap() {
        message_box(null_mut(), &e, MB_ICONERROR);
        std::process::exit(1);
    }
    with_app(|a| {
        a.tick(true);
        a.add_tray_icon();
        a.promote_once();
        a.sync_app_autostart();
        if a.config.autoboot {
            a.start();
        }
        unsafe { SetTimer(a.hwnd.get(), TIMER_POLL, opts.poll.as_millis() as u32, None) };
    });

    // Standard message loop. GetMessageW returns 0 on WM_QUIT and -1 on error;
    // both end the program.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl App {
    /// Registers the window class and creates the hidden window.
    ///
    /// It is a normal top-level window with no style bits and no `ShowWindow`
    /// call, so it never appears. A message-only window (`HWND_MESSAGE`)
    /// would not do: `TrackPopupMenuEx` needs an owner that can be brought to
    /// the foreground, otherwise the menu does not close when the user clicks
    /// elsewhere.
    fn create_window(&self) -> Result<(), String> {
        unsafe {
            let hinst = GetModuleHandleW(null());
            self.taskbar_created
                .set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));

            let class_name = wide(CLASS_NAME);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: null_mut(),
            };
            if RegisterClassExW(&wc) == 0 {
                return Err(format!("RegisterClassEx failed ({})", GetLastError()));
            }
            // A hidden top-level window: needed to receive tray callbacks and to
            // own the popup menu (message-only windows cannot be brought to
            // the foreground).
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                wide(APP_TITLE).as_ptr(),
                0,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                null_mut(),
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                return Err(format!("CreateWindowEx failed ({})", GetLastError()));
            }
            self.hwnd.set(hwnd);
            Ok(())
        }
    }

    /// Message handler. Returns `Some(result)` for messages it consumed and
    /// `None` to let `DefWindowProcW` handle everything else.
    fn handle(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TRAY_CALLBACK => {
                // NOTIFYICON_VERSION_4 semantics: left click -> NIN_SELECT,
                // keyboard -> NIN_KEYSELECT, right click -> WM_CONTEXTMENU. The
                // raw mouse messages also arrive and are deliberately ignored
                // to avoid opening the menu twice.
                match (lparam & 0xFFFF) as u32 {
                    WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => self.show_menu(),
                    _ => {}
                }
                Some(0)
            }
            WM_TIMER => {
                if wparam == TIMER_POLL {
                    self.tick(false);
                    self.promote_once();
                }
                Some(0)
            }
            WM_REFRESH_NOW => {
                // The Start/Restart/Shutdown thread is done; re-enable the
                // menu items and show the new state right away.
                self.busy.set(Busy::Idle);
                self.tick(true);
                Some(0)
            }
            WM_CLOSE => {
                unsafe { DestroyWindow(hwnd) };
                Some(0)
            }
            WM_DESTROY => {
                // WSL2 itself is left exactly as it is: this app keeps no
                // process of its own running to hold it up, see the module
                // docs. Tear down in reverse order of creation, then end the
                // loop.
                unsafe {
                    KillTimer(hwnd, TIMER_POLL);
                    Shell_NotifyIconW(NIM_DELETE, &*self.nid.borrow());
                    let h = self.hicon.replace(null_mut());
                    if !h.is_null() {
                        DestroyIcon(h);
                    }
                    PostQuitMessage(0);
                }
                Some(0)
            }
            m if m != 0 && m == self.taskbar_created.get() => {
                // Explorer was restarted (or crashed): its tray forgot us.
                self.add_tray_icon();
                Some(0)
            }
            _ => None,
        }
    }

    /// One poll cycle: asks the [`Monitor`] for the current state and, if
    /// anything visible changed (or the icon does not exist yet), updates
    /// the icon and tooltip. `force` bypasses the stats interval.
    fn tick(&self, force: bool) {
        let (st, changed) = self.mon.borrow_mut().poll(force);
        log!(
            "poll force={force} -> running={} pid={} cpu={:.2} mem={} changed={changed}",
            st.running,
            st.pid,
            st.cpu.unwrap_or(-1.0),
            format_bytes(st.mem)
        );
        if changed || self.hicon.get().is_null() {
            self.update_icon(&st);
        }
        trim_working_set();
    }

    /// Picks the icon to show for `running`, per the `icon` config key:
    /// `"color"`/`"mono"` pin it, anything else (including empty) follows
    /// WSL2's state.
    fn icon_kind(&self, running: bool) -> IconKind {
        match self.config.icon.to_ascii_lowercase().as_str() {
            "color" => IconKind::Color,
            "mono" => IconKind::Mono,
            _ if running => IconKind::Color,
            _ => IconKind::Mono,
        }
    }

    /// Pushes `st` to the shell: reloads the icon if which one should be
    /// shown changed, rewrites the tooltip, and calls `NIM_MODIFY`.
    ///
    /// The previous icon is destroyed only after the new one has been
    /// created; the shell copies the icon during `NIM_MODIFY`, so destroying
    /// the old handle afterwards is safe.
    fn update_icon(&self, st: &Status) {
        let kind = self.icon_kind(st.running);
        if self.last_icon.get() != Some(kind) || self.hicon.get().is_null() {
            if let Ok(h) = icon::load(kind, self.icon_size) {
                let old = self.hicon.replace(h);
                if !old.is_null() {
                    unsafe { DestroyIcon(old) };
                }
                self.last_icon.set(Some(kind));
            }
        }
        let mut nid = self.nid.borrow_mut();
        set_tip(&mut nid, &tooltip(st, self.busy.get()));
        nid.hIcon = self.hicon.get();
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &*nid) };
    }

    /// Redraws the tooltip (and, if needed, the icon) right away. Used after
    /// changing [`App::busy`] outside of [`App::tick`], since a `busy`
    /// transition is not itself a change in [`Status`] and would otherwise
    /// wait for the next one to show up.
    fn refresh_display(&self) {
        let st = self.mon.borrow().current();
        self.update_icon(&st);
    }

    /// Adds the icon to the notification area and switches it to
    /// `NOTIFYICON_VERSION_4` behaviour, under which the shell sends
    /// `NIN_SELECT` / `NIN_KEYSELECT` / `WM_CONTEXTMENU` in `lParam`'s low
    /// word instead of raw mouse messages only. Also used after an Explorer
    /// restart, hence the full re-initialisation of `nid`.
    fn add_tray_icon(&self) {
        let mut nid = self.nid.borrow_mut();
        *nid = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd.get();
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.hIcon = self.hicon.get();
        let st = self.mon.borrow().current();
        set_tip(&mut nid, &tooltip(&st, self.busy.get()));
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &*nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &*nid);
        }
    }

    /// Builds and shows the popup menu at the cursor, then runs the chosen
    /// command. Used for left click, right click and keyboard activation.
    /// Status (Running/Stopped, CPU, RAM) is shown in the tooltip only; see
    /// [`tooltip`].
    ///
    /// The menu is rebuilt on every click because its contents (the enabled
    /// states) depend on the current state.
    fn show_menu(&self) {
        // TrackPopupMenuEx pumps messages, so a second tray click would land
        // here again while the first menu is still open.
        if self.menu_open.replace(true) {
            return;
        }
        let st = self.mon.borrow().current();
        let busy = self.busy.get();
        let cmd = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                self.menu_open.set(false);
                return;
            }
            let add = |flags: u32, id: usize, text: &str| {
                AppendMenuW(menu, flags, id, wide(text).as_ptr());
            };
            // Enabled only when `available` (e.g. Start needs WSL2 stopped)
            // and no other command is already in flight. Explorer and
            // Terminal reuse it too: both need a running, idle session.
            let action = |available: bool| {
                MF_STRING
                    | if available && busy == Busy::Idle {
                        0
                    } else {
                        MF_GRAYED
                    }
            };

            add(action(!st.running), IDM_START, "&Start");
            add(action(st.running), IDM_RESTART, "&Restart");
            add(action(st.running), IDM_SHUTDOWN, "S&hutdown");
            add(MF_SEPARATOR, 0, "");
            add(action(st.running), IDM_EXPLORER, "&Explorer");
            add(action(st.running), IDM_TERMINAL, "Ter&minal");
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, IDM_EXIT, "E&xit");

            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let hwnd = self.hwnd.get();
            // The documented tray-menu dance (KB 135788): the owner must be the
            // foreground window or the menu will not close when the user
            // clicks elsewhere, and posting a no-op message afterwards makes
            // the menu go away promptly once the next click lands.
            SetForegroundWindow(hwnd);
            // TPM_BOTTOMALIGN: the menu opens upwards from the taskbar.
            // TPM_RETURNCMD: the chosen id is returned instead of a WM_COMMAND.
            let cmd = TrackPopupMenuEx(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
                pt.x,
                pt.y,
                hwnd,
                null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            cmd as usize // 0 = dismissed without a choice
        };

        log!("menu command {cmd}");
        match cmd {
            IDM_START => self.start(),
            IDM_RESTART => self.restart(),
            IDM_SHUTDOWN => self.shutdown(),
            IDM_EXPLORER => self.open_explorer(),
            IDM_TERMINAL => self.open_terminal(),
            IDM_EXIT => unsafe {
                DestroyWindow(self.hwnd.get());
            },
            _ => {}
        }
        // Released only now: the message boxes shown by the commands above pump
        // messages too, and a tray click during them must not open another menu.
        self.menu_open.set(false);
    }

    /// "Start": boots the configured distribution (or WSL's own default) by
    /// running `wsl.exe ... -- exit` on a helper thread, exactly as typing
    /// that into Win+R would, and returns as soon as it exits. A no-op while
    /// WSL2 is already running or another command is in flight. The thread
    /// reports back with `WM_REFRESH_NOW`.
    fn start(&self) {
        if self.busy.get() != Busy::Idle || self.mon.borrow().current().running {
            return;
        }
        self.busy.set(Busy::Starting);
        self.refresh_display();
        let hwnd = self.hwnd.get() as isize;
        let distro = self.config.distroname.clone();
        std::thread::spawn(move || {
            if let Err(e) = start_wsl(&distro) {
                message_box(null_mut(), &e, MB_ICONERROR);
            }
            unsafe { PostMessageW(hwnd as HWND, WM_REFRESH_NOW, 0, 0) };
        });
    }

    /// "Shutdown": asks for confirmation (default button is No), then runs
    /// `wsl --shutdown` on a helper thread so the UI keeps responding. The
    /// thread reports back with `WM_REFRESH_NOW`.
    fn shutdown(&self) {
        if self.busy.get() != Busy::Idle {
            return;
        }
        let r = message_box_titled(
            self.hwnd.get(),
            "Shutdown WSL",
            "Confirm shutting down WSL?\n\nAll running distributions will be terminated.",
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        );
        if r != IDYES {
            return;
        }
        self.busy.set(Busy::ShuttingDown);
        self.refresh_display();
        // HWND is a raw pointer and therefore not Send; carry it as an integer.
        // The thread only ever hands it to PostMessageW, which is thread-safe.
        let hwnd = self.hwnd.get() as isize;
        std::thread::spawn(move || {
            if let Err(e) = shutdown_wsl() {
                message_box(null_mut(), &e, MB_ICONERROR);
            }
            unsafe { PostMessageW(hwnd as HWND, WM_REFRESH_NOW, 0, 0) };
        });
    }

    /// "Restart": shuts WSL2 down (`wsl --shutdown`) and boots it again
    /// (`wsl.exe ... -- exit`), both on the same helper thread so the two
    /// steps run in order. Never prompts: choosing Restart from the menu is
    /// itself an explicit, deliberate action. The thread reports back with
    /// `WM_REFRESH_NOW`.
    fn restart(&self) {
        if self.busy.get() != Busy::Idle {
            return;
        }
        self.busy.set(Busy::Restarting);
        self.refresh_display();
        let hwnd = self.hwnd.get() as isize;
        let distro = self.config.distroname.clone();
        std::thread::spawn(move || {
            if let Err(e) = shutdown_wsl() {
                message_box(null_mut(), &e, MB_ICONERROR);
            }
            if let Err(e) = start_wsl(&distro) {
                message_box(null_mut(), &e, MB_ICONERROR);
            }
            unsafe { PostMessageW(hwnd as HWND, WM_REFRESH_NOW, 0, 0) };
        });
    }

    /// "Explorer": opens `\\wsl.localhost\<distro>`, or `\\wsl.localhost`
    /// (letting Explorer list every distribution) when none is configured.
    /// Only reachable from the menu while WSL2 is running and idle; see
    /// [`App::show_menu`].
    fn open_explorer(&self) {
        let target = if self.config.distroname.is_empty() {
            r"\\wsl.localhost".to_string()
        } else {
            format!(r"\\wsl.localhost\{}", self.config.distroname)
        };
        shell_open(self.hwnd.get(), &target, "");
    }

    /// "Terminal": with `wtprofile` configured, opens that Windows Terminal
    /// profile directly (`wt.exe --profile <name>`); WSL profiles already
    /// default to the Linux user's home directory, so nothing else is
    /// needed. Otherwise opens the system's default terminal running
    /// `wsl --distribution <distro>` (or plain `wsl` with no distribution
    /// configured), exactly as typing that into Win+R would, except that
    /// `--cd ~` is always added so the session starts in the Linux user's
    /// home directory rather than wherever this app's own working directory
    /// happens to map to. Only reachable from the menu while WSL2 is running
    /// and idle; see [`App::show_menu`].
    fn open_terminal(&self) {
        if !self.config.wtprofile.is_empty() {
            let params = format!("--profile {}", quote_arg(&self.config.wtprofile));
            shell_open(self.hwnd.get(), "wt.exe", &params);
            return;
        }
        let mut params = String::from("--cd ~");
        if !self.config.distroname.is_empty() {
            params = format!(
                "--distribution {} {params}",
                quote_arg(&self.config.distroname)
            );
        }
        shell_open(self.hwnd.get(), "wsl.exe", &params);
    }

    // ---- Windows 11 tray promotion ----

    /// Asks Windows 11 to show the icon next to the clock instead of in the
    /// overflow flyout. Explorer keeps per-icon settings under
    /// `HKCU\Control Panel\NotifyIconSettings\<id>` with ExecutablePath and
    /// IsPromoted. The entry appears shortly after the icon is added, so this
    /// is retried a few times. IsPromoted is only written when absent, so a
    /// later manual choice in Settings > Taskbar is respected.
    fn promote_once(&self) {
        if self.promoted.get() || self.promote_tries.get() > 6 || self.exe_path.is_empty() {
            return;
        }
        self.promote_tries.set(self.promote_tries.get() + 1);
        self.promoted.set(promote_tray_icon(&self.exe_path));
    }

    // ---- autostart (HKCU\...\Run) ----

    /// Applies `autostart` from the config file to the `Run` key, every
    /// launch. The config file is the only place this is set (there is no
    /// menu toggle), so the registry is kept in sync with it unconditionally
    /// rather than only on a user-initiated change.
    fn sync_app_autostart(&self) {
        if let Err(e) = self.set_autostart(self.config.autostart) {
            log!("autostart sync failed: {e}");
        }
    }

    /// Writes or deletes the `Run` value. The value is the quoted launch path
    /// followed by this instance's own arguments, so `-poll`/`-log`/`-config`
    /// settings survive into the autostarted copy.
    fn set_autostart(&self, enable: bool) -> Result<(), String> {
        let key = RegKey::open(HKEY_CURRENT_USER, RUN_KEY, KEY_READ | KEY_WRITE)?;
        if !enable {
            unsafe { RegDeleteValueW(key.0, wide(RUN_VALUE).as_ptr()) };
            return Ok(());
        }
        // The launch path (GetModuleFileName) as-is, like the Go version: a
        // subst drive or junction the user launched through stays in the value.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut val = format!("\"{}\"", exe.display());
        if !self.launch_args.is_empty() {
            val.push(' ');
            val.push_str(&self.launch_args.join(" "));
        }
        key.set_string(RUN_VALUE, &val)
            .map_err(|r| format!("cannot write Run value ({r})"))
    }
}

/// The window procedure registered in [`App::create_window`]. Forwards to
/// [`App::handle`]; anything not handled there goes to `DefWindowProcW`.
extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(Some(r)) = with_app(|a| a.handle(hwnd, msg, wparam, lparam)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Status line shared by the tooltip and the menu: a transitional label
/// while Start/Restart/Shutdown is in flight, otherwise `Running`/`Stopped`.
fn status_text(running: bool, busy: Busy) -> &'static str {
    match busy {
        Busy::Starting => "Starting...",
        Busy::Restarting => "Restarting...",
        Busy::ShuttingDown => "Shutting down...",
        Busy::Idle if running => "Running",
        Busy::Idle => "Stopped",
    }
}

/// Tooltip text: the status line, and CPU/RAM only while idle and running
/// (a transitional label, or `Stopped`, never has numbers to go with it).
fn tooltip(st: &Status, busy: Busy) -> String {
    let label = status_text(st.running, busy);
    if busy != Busy::Idle || !st.running {
        return label.into();
    }
    format!("{label}\n{}\n{}", cpu_label(st), ram_label(st))
}

/// `CPU: 12.4%`; shows `CPU: ...` until the second sample after the VM
/// appeared.
fn cpu_label(st: &Status) -> String {
    match st.cpu {
        Some(c) => format!("CPU: {c:.1}%"),
        None => "CPU: ...".into(),
    }
}

/// `RAM: 5.00 GB` (or `RAM: 768 MB` below 1 GiB).
fn ram_label(st: &Status) -> String {
    format!("RAM: {}", format_bytes(st.mem))
}

/// `ShellExecuteW("open", file, params)`, used by the Explorer and Terminal
/// menu commands. Errors are shown in a message box; there is no result the
/// caller needs afterwards.
fn shell_open(hwnd: HWND, file: &str, params: &str) {
    // Win32 SW_SHOWNORMAL; not imported from windows-sys because ShellExecuteW
    // takes a plain i32 here, not the SHOW_WINDOW_CMD type ShowWindow uses.
    const SW_SHOWNORMAL: i32 = 1;
    let file_w = wide(file);
    let params_w = wide(params);
    let result = unsafe {
        ShellExecuteW(
            hwnd,
            wide("open").as_ptr(),
            file_w.as_ptr(),
            if params.is_empty() {
                null()
            } else {
                params_w.as_ptr()
            },
            null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a pseudo-HINSTANCE: a value above 32 means success.
    if (result as isize) <= 32 {
        message_box(
            hwnd,
            &format!("Could not open {file}: error code {}", result as isize),
            MB_ICONERROR,
        );
    }
}

/// Copies `s` into `szTip` (128 UTF-16 units including the terminator),
/// truncating if needed.
fn set_tip(nid: &mut NOTIFYICONDATAW, s: &str) {
    let mut u: Vec<u16> = s.encode_utf16().collect();
    u.truncate(nid.szTip.len() - 1);
    nid.szTip.fill(0);
    nid.szTip[..u.len()].copy_from_slice(&u);
}

// ---- registry helpers ----

/// An open registry key, closed on drop. Only the handful of operations the
/// program needs are wrapped.
struct RegKey(HKEY);

impl RegKey {
    /// Opens `root\sub` with the given `KEY_*` access mask.
    fn open(root: HKEY, sub: &str, access: u32) -> Result<RegKey, String> {
        let mut h: HKEY = null_mut();
        let r = unsafe { RegOpenKeyExW(root, wide(sub).as_ptr(), 0, access, &mut h) };
        if r != 0 {
            return Err(format!("cannot open {sub} ({r})"));
        }
        Ok(RegKey(h))
    }

    /// Reads a `REG_SZ` / `REG_EXPAND_SZ` value (unexpanded, up to 1023
    /// characters). `None` if absent or of another type.
    fn read_string(&self, name: &str) -> Option<String> {
        let mut typ = 0u32;
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                null_mut(),
                &mut typ,
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if r != 0 || (typ != REG_SZ && typ != REG_EXPAND_SZ) {
            return None;
        }
        let n = (len as usize / 2).min(buf.len());
        let end = buf[..n].iter().position(|&c| c == 0).unwrap_or(n);
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    /// Reads a `REG_DWORD` value; `None` if absent or of another type.
    fn read_dword(&self, name: &str) -> Option<u32> {
        let mut typ = 0u32;
        let mut v = 0u32;
        let mut len = 4u32;
        let r = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                null_mut(),
                &mut typ,
                (&mut v as *mut u32).cast(),
                &mut len,
            )
        };
        (r == 0 && typ == REG_DWORD).then_some(v)
    }

    /// Writes a `REG_SZ` value. Fails with the Win32 error code.
    fn set_string(&self, name: &str, value: &str) -> Result<(), u32> {
        let u = wide(value);
        let r = unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                u.as_ptr().cast(),
                (u.len() * 2) as u32,
            )
        };
        if r != 0 {
            return Err(r);
        }
        Ok(())
    }

    /// Writes a `REG_DWORD` value; failure is ignored (best effort).
    fn set_dword(&self, name: &str, value: u32) {
        unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            );
        }
    }

    /// Name of the `index`-th subkey, or `None` past the end (key names are
    /// at most 255 characters, so the fixed buffer always suffices).
    fn subkey_name(&self, index: u32) -> Option<String> {
        let mut name = [0u16; 256];
        let mut n = name.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                self.0,
                index,
                name.as_mut_ptr(),
                &mut n,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        (r == 0).then(|| String::from_utf16_lossy(&name[..n as usize]))
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// Maps the extended-length syntax returned by `canonicalize` back to the
/// Win32 form Explorer records: `\\?\C:\x` -> `C:\x`,
/// `\\?\UNC\server\share\x` -> `\\server\share\x`. Anything else is kept.
fn win32_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match p.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => p.to_string(),
    }
}

/// Windows 11 hides new tray icons in the overflow flyout by default. The
/// per-icon choice lives under `HKCU\Control Panel\NotifyIconSettings\<id>`,
/// where Explorer records `ExecutablePath` and (once the user has decided)
/// `IsPromoted`. Setting `IsPromoted = 1` ourselves shows the icon next to
/// the clock, and Explorer picks the change up live.
///
/// Only an *absent* value is written: if the user has already hidden or shown
/// the icon in Settings › Taskbar, that choice stands.
///
/// Returns true once the entry was found and handled, or when this Windows
/// version has no such key (Windows 10). Explorer creates the entry a little
/// after `NIM_ADD`, so the caller retries on the next timer ticks.
fn promote_tray_icon(exe: &str) -> bool {
    const BASE: &str = r"Control Panel\NotifyIconSettings";
    let Ok(root) = RegKey::open(HKEY_CURRENT_USER, BASE, KEY_READ) else {
        return true; // no per-icon settings on this Windows version; nothing to do
    };
    let mut i = 0;
    while let Some(sub) = root.subkey_name(i) {
        i += 1;
        let Ok(k) = RegKey::open(
            HKEY_CURRENT_USER,
            &format!(r"{BASE}\{sub}"),
            KEY_READ | KEY_WRITE,
        ) else {
            continue;
        };
        if k.read_string("ExecutablePath")
            .is_some_and(|p| p.eq_ignore_ascii_case(exe))
        {
            if k.read_dword("IsPromoted").is_none() {
                k.set_dword("IsPromoted", 1);
            }
            return true;
        }
    }
    false // not found (yet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("1h5m"), Some(Duration::from_secs(3900)));
        assert_eq!(parse_duration("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration(".5s"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("500us"), Some(Duration::from_micros(500)));
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("30"), None); // missing unit
        assert_eq!(parse_duration("1m30"), None);
        assert_eq!(parse_duration("-5s"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration(".s"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("99999999999999999999h"), None); // overflow
    }

    #[test]
    fn win32_paths() {
        assert_eq!(
            win32_path(r"\\?\C:\Tools\wsltray.exe"),
            r"C:\Tools\wsltray.exe"
        );
        assert_eq!(
            win32_path(r"\\?\UNC\server\share\wsltray.exe"),
            r"\\server\share\wsltray.exe"
        );
        assert_eq!(win32_path(r"C:\Tools\wsltray.exe"), r"C:\Tools\wsltray.exe");
        assert_eq!(win32_path(r"\\?\Volume{1}\x.exe"), r"\\?\Volume{1}\x.exe");
    }

    #[test]
    fn tooltip_fits() {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        set_tip(&mut nid, &"x".repeat(500));
        assert_eq!(nid.szTip[126], 'x' as u16);
        assert_eq!(nid.szTip[127], 0);
        set_tip(&mut nid, "short");
        assert_eq!(nid.szTip[5], 0);
    }

    #[test]
    fn stats_text() {
        let st = Status {
            running: true,
            cpu: Some(12.44),
            mem: 5 << 30,
            mem_pct: 8.44,
            ..Default::default()
        };
        assert_eq!(cpu_label(&st), "CPU: 12.4%");
        assert_eq!(ram_label(&st), "RAM: 5.00 GB");
        assert_eq!(cpu_label(&Status { cpu: None, ..st }), "CPU: ...");
        assert_eq!(
            tooltip(&st, Busy::Idle),
            "Running\nCPU: 12.4%\nRAM: 5.00 GB"
        );
        assert_eq!(tooltip(&Status::default(), Busy::Idle), "Stopped");
    }

    #[test]
    fn tooltip_hides_stats_while_busy() {
        let st = Status {
            running: true,
            cpu: Some(12.44),
            mem: 5 << 30,
            ..Default::default()
        };
        assert_eq!(tooltip(&st, Busy::Starting), "Starting...");
        assert_eq!(tooltip(&st, Busy::Restarting), "Restarting...");
        assert_eq!(tooltip(&st, Busy::ShuttingDown), "Shutting down...");
        assert_eq!(status_text(false, Busy::Idle), "Stopped");
        assert_eq!(status_text(true, Busy::Idle), "Running");
    }
}
