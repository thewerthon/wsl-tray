//! Finds the WSL2 utility VM and measures it.
//!
//! # Why the process list, and why this API
//!
//! WSL2 runs all distributions inside one lightweight Hyper-V VM whose memory
//! and CPU time Windows accounts to a placeholder process, `vmmemWSL`
//! (`vmmem` on Windows 10). Its existence is the most reliable "is WSL2 on"
//! signal: `wsl --list --running` reports no running distributions while the
//! VM is still alive and holding memory (the VM lingers for `vmIdleTimeout`
//! after the last distribution exits).
//!
//! The VM process runs as SYSTEM, so `OpenProcess` on it fails for a normal
//! user with every access mask, and `GetProcessTimes` / `GetProcessMemoryInfo`
//! are out. `NtQuerySystemInformation(SystemSessionProcessInformation)`
//! returns the same numbers for every process of one session without opening
//! anything, which is how Task Manager does it too; restricting the query to
//! session 0, where the WSL service (and therefore the VM it creates) always
//! runs, keeps the snapshot small. It is an undocumented-but-stable ntdll
//! export; the function is resolved with `GetProcAddress` so no import
//! library is needed.
//!
//! # What the numbers mean
//!
//! * CPU: the increase of the process's kernel+user time between two
//!   samples, divided by the wall-clock time between them and by the number
//!   of logical cores, in percent. 100 % means every core of the host was busy
//!   with WSL2. It is `None` until a second sample exists.
//! * Memory: the process working set, which for the VM is the memory the VM
//!   has actually touched; this is the "Memory" column Task Manager shows for
//!   `vmmemWSL`. Also given as a percentage of physical RAM.
//!
//! # Cost
//!
//! A session-0 snapshot copies only the service processes: a few hundred KB
//! instead of the roughly 830 KB and 250+ entries a full-system snapshot
//! would need, since the VM is created by the WSL service and always lives
//! in session 0. The buffer is allocated fresh for each snapshot, sized from
//! what the previous one needed, and dropped once its entries are walked, so
//! it is not part of the process's resident memory between polls, which
//! happen every `-poll` seconds (`refreshms` in the config file).

use std::ffi::c_void;
use std::ptr::null;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemDirectoryW, GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW, INFINITE,
    PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::wide;

/// One observation of the VM, as shown by the UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    /// The VM process exists.
    pub running: bool,
    /// Process id of the VM (0 when not running). A changed pid means the VM
    /// was restarted, which resets the CPU baseline.
    pub pid: usize,
    /// Percent of all host logical cores; `None` = not measured yet.
    pub cpu: Option<f64>,
    /// Working set of the WSL2 VM process, bytes.
    pub mem: u64,
    /// `mem` as a percentage of the host's physical RAM.
    pub mem_pct: f64,
}

/// Stateful sampler: remembers the previous CPU reading so the next one can
/// be turned into a rate, and how large the last process snapshot was.
pub struct Monitor {
    /// Image name to look for, lower-cased with full Unicode rules.
    proc_name: String,
    /// Logical cores of the host, the denominator of the CPU percentage.
    ncpu: f64,
    /// Physical RAM in bytes, the denominator of the memory percentage.
    total_mem: f64,

    /// Last published status.
    cur: Status,
    /// When `last_cpu` was read (the CPU baseline).
    last_t: Instant,
    /// Kernel+user time of the VM at `last_t`, in 100 ns units.
    last_cpu: i64,
    /// Pid the baseline belongs to; a different pid invalidates it.
    last_pid: usize,
    /// Size to allocate for the next snapshot buffer: what the last one
    /// needed, plus slack for processes started since.
    snapshot_size: usize,
    /// `ntdll!NtQuerySystemInformation`, or `None` if it could not be found
    /// (then the VM is reported as not running).
    nt_query: Option<NtQuerySystemInformationFn>,
}

/// `NTSTATUS NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS, PVOID, ULONG, PULONG)`.
type NtQuerySystemInformationFn = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

/// `SystemSessionProcessInformation` member of `SYSTEM_INFORMATION_CLASS`:
/// the process list of one session, requested through [`SysSessionProcInfo`].
const SYSTEM_SESSION_PROCESS_INFORMATION: u32 = 53;
/// Session the VM process runs in: it is created by the WSL service, which
/// like every service runs in session 0.
const VM_SESSION: u32 = 0;
/// Returned when the buffer is too small; `needed` then holds the size.
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004_u32 as i32;
/// Slack added to the size the last snapshot needed, for processes started
/// since; a snapshot that still does not fit is retried with the new size.
const SNAPSHOT_SLACK: usize = 64 * 1024;

/// `SYSTEM_SESSION_PROCESS_INFORMATION`: the input of a session-restricted
/// snapshot. `buffer` receives the same chain of entries a full-system
/// snapshot would, just limited to `session_id`.
#[repr(C)]
struct SysSessionProcInfo {
    session_id: u32,
    size_of_buf: u32,
    buffer: *mut c_void,
}

/// Leading part of `SYSTEM_PROCESS_INFORMATION` (x64 layout, from the Windows
/// SDK's `winternl.h` plus the documented "reserved" fields). Entries are
/// chained by `next_entry_offset`; each is followed by its thread array and
/// the image-name string, which is why only the prefix is declared and
/// entries are read with `read_unaligned` at their offsets.
#[repr(C)]
struct SysProcInfo {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name_length: u16,
    image_name_max_length: u16,
    _pad0: [u8; 4],
    image_name: *const u16,
    base_priority: i32,
    _pad1: [u8; 4],
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    _pad2: [u8; 4],
    peak_working_set_size: usize,
    working_set_size: usize,
}

impl Monitor {
    /// Creates a sampler for the process called `proc_name`. Captures the
    /// host's core count and RAM once; both are constant for the session.
    pub fn new(proc_name: &str) -> Self {
        let nt_query = unsafe {
            let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
            GetProcAddress(ntdll, c"NtQuerySystemInformation".as_ptr().cast())
                .map(|f| std::mem::transmute::<_, NtQuerySystemInformationFn>(f))
        };
        Monitor {
            proc_name: proc_name.chars().flat_map(char::to_lowercase).collect(),
            ncpu: num_cpus() as f64,
            total_mem: total_phys_mem() as f64,
            cur: Status::default(),
            last_t: Instant::now(),
            last_cpu: 0,
            last_pid: 0,
            snapshot_size: 256 * 1024,
            nt_query,
        }
    }

    /// The status published by the last [`poll`](Self::poll).
    pub fn current(&self) -> Status {
        self.cur
    }

    /// Takes a process snapshot and updates the status.
    ///
    /// Returns the status and whether anything visible changed, so the caller
    /// can skip redrawing. The state machine:
    ///
    /// * VM not found: report stopped (changed only if it was running).
    /// * VM newly found, or a different pid than last time: publish memory
    ///   immediately, remember the CPU time as a baseline, CPU stays `None`.
    /// * VM known: memory is always refreshed; CPU is recomputed too, unless
    ///   the window since the baseline is shorter than one second, which
    ///   would give a meaningless rate.
    pub fn poll(&mut self) -> (Status, bool) {
        let now = Instant::now();
        let found = self.find();

        let Some((pid, cpu_time, ws)) = found else {
            let changed = self.cur.running;
            self.cur = Status::default();
            self.last_pid = 0;
            return (self.cur, changed);
        };

        // Newly started (or restarted) VM: establish a CPU baseline, show memory now.
        if !self.cur.running || pid != self.last_pid {
            self.cur = Status {
                running: true,
                pid,
                cpu: None,
                mem: ws,
                mem_pct: self.mem_pct(ws),
            };
            self.last_pid = pid;
            self.last_t = now;
            self.last_cpu = cpu_time;
            return (self.cur, true);
        }

        // Steady state: refresh on every call. A window shorter than one
        // second gives a meaningless CPU rate, so that part is skipped; memory
        // is cheap and always current.
        let elapsed = now.duration_since(self.last_t);
        if elapsed >= Duration::from_secs(1) {
            // too short a window gives meaningless numbers
            let delta_ns = (cpu_time - self.last_cpu) as f64 * 100.0; // 100 ns units -> ns
            let pct = delta_ns / elapsed.as_nanos() as f64 / self.ncpu * 100.0;
            self.cur.cpu = Some(pct.max(0.0));
            self.last_t = now;
            self.last_cpu = cpu_time;
        }
        self.cur.mem = ws;
        self.cur.mem_pct = self.mem_pct(ws);
        (self.cur, true)
    }

    /// `bytes` as a percentage of physical RAM (0 if RAM could not be read).
    fn mem_pct(&self, bytes: u64) -> f64 {
        if self.total_mem <= 0.0 {
            return 0.0;
        }
        bytes as f64 / self.total_mem * 100.0
    }

    /// Takes a snapshot of the session-0 process list (into a buffer
    /// allocated for the call and sized from the previous one, grown if
    /// `STATUS_INFO_LENGTH_MISMATCH` says so) and walks the entries. Returns
    /// `(pid, kernel+user time in 100 ns units, working set bytes)` of the
    /// first process whose image name matches, case-insensitively.
    fn find(&mut self) -> Option<(usize, i64, u64)> {
        let query = self.nt_query?;
        let mut buf = vec![0u8; self.snapshot_size];
        loop {
            let mut needed: u32 = 0;
            let mut req = SysSessionProcInfo {
                session_id: VM_SESSION,
                size_of_buf: buf.len() as u32,
                buffer: buf.as_mut_ptr().cast(),
            };
            let st = unsafe {
                query(
                    SYSTEM_SESSION_PROCESS_INFORMATION,
                    (&mut req as *mut SysSessionProcInfo).cast(),
                    std::mem::size_of::<SysSessionProcInfo>() as u32,
                    &mut needed,
                )
            };
            if st == 0 {
                break;
            }
            if st != STATUS_INFO_LENGTH_MISMATCH {
                return None;
            }
            // `needed` is the size that would have fitted; doubling as a
            // floor guarantees progress even if it were left at zero.
            buf = vec![0; (needed as usize + SNAPSHOT_SLACK).max(buf.len() * 2)];
        }
        self.snapshot_size = buf.len();

        let mut off = 0usize;
        loop {
            if off + std::mem::size_of::<SysProcInfo>() > buf.len() {
                return None;
            }
            // SAFETY: the kernel filled buf with a chain of SYSTEM_PROCESS_INFORMATION
            // entries; each entry is at least size_of::<SysProcInfo>() bytes.
            let p =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const SysProcInfo) };
            if !p.image_name.is_null() && p.image_name_length > 0 {
                let n = p.image_name_length as usize / 2;
                // SAFETY: image_name points into buf (the kernel stores the string
                // after the fixed part of the entry), n UTF-16 units long.
                let name = unsafe { std::slice::from_raw_parts(p.image_name, n) };
                if eq_lowercase_utf16(name, &self.proc_name) {
                    return Some((
                        p.unique_process_id,
                        p.user_time + p.kernel_time,
                        p.working_set_size as u64,
                    ));
                }
            }
            if p.next_entry_offset == 0 {
                return None;
            }
            off += p.next_entry_offset as usize;
        }
    }
}

/// Compares a UTF-16 image name against a pre-lowercased `&str`, lowering
/// every character (not just A-Z) the way Go's `strings.ToLower` does, and
/// without allocating per process.
fn eq_lowercase_utf16(name: &[u16], lower: &str) -> bool {
    char::decode_utf16(name.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .flat_map(char::to_lowercase)
        .eq(lower.chars())
}

/// Logical processor count from `GetSystemInfo` (at least 1).
fn num_cpus() -> u32 {
    let mut si: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut si) };
    si.dwNumberOfProcessors.max(1)
}

/// Physical RAM in bytes from `GlobalMemoryStatusEx`, or 0 on failure.
fn total_phys_mem() -> u64 {
    let mut ms: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut ms) } == 0 {
        return 0;
    }
    ms.ullTotalPhys
}

/// Resolves `%SystemRoot%\System32\wsl.exe` as a NUL-terminated wide string.
/// Used instead of a bare `wsl.exe` command line so the current directory is
/// never searched for one.
fn wsl_exe_path() -> Result<Vec<u16>, String> {
    let mut dir = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(dir.as_mut_ptr(), dir.len() as u32) } as usize;
    if n == 0 || n >= dir.len() {
        return Err(format!("GetSystemDirectory failed ({})", unsafe {
            GetLastError()
        }));
    }
    let mut exe = dir[..n].to_vec();
    exe.extend(wide(r"\wsl.exe"));
    Ok(exe)
}

/// Starts `wsl.exe <args>` with no visible console window and returns its
/// process/thread handles without waiting for it.
///
/// Uses `CreateProcessW` directly instead of `std::process::Command`: the
/// latter pulls in about 67 KB of pipe and environment handling that is
/// never needed here.
fn spawn_wsl(args: &str) -> Result<PROCESS_INFORMATION, String> {
    let exe = wsl_exe_path()?;
    let mut cmdline = wide(&format!("wsl.exe {args}")); // CreateProcessW may modify it
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            exe.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW,
            null(),
            null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(format!(
            "wsl.exe {args}: CreateProcess failed ({})",
            unsafe { GetLastError() }
        ));
    }
    Ok(pi)
}

/// Runs `wsl.exe <args>` to completion and fails with its exit code if it is
/// non-zero. Blocks for as long as the command takes, so callers run this on
/// a helper thread, never on the UI thread.
fn run_wsl(args: &str) -> Result<(), String> {
    let pi = spawn_wsl(args)?;
    unsafe {
        CloseHandle(pi.hThread);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        if code != 0 {
            return Err(format!("wsl.exe {args}: exit status {code}"));
        }
    }
    Ok(())
}

/// Runs `wsl.exe --shutdown`: stops the WSL2 VM and every distribution
/// running in it.
pub fn shutdown_wsl() -> Result<(), String> {
    run_wsl("--shutdown")
}

/// Boots the WSL2 VM (and, if given, a specific distribution) by running
/// `wsl.exe [--distribution <distro>] -- exit`, exactly as typing that into
/// Win+R would: waits for the trivial `exit` command to return and then
/// leaves no process of its own running. Whether the VM stays up afterwards
/// is up to `.wslconfig`'s own `vmIdleTimeout`, not this app. An empty
/// `distro` omits `--distribution`, so WSL starts whichever distribution it
/// picks by default.
pub fn start_wsl(distro: &str) -> Result<(), String> {
    let args = if distro.is_empty() {
        "-- exit".to_string()
    } else {
        format!("--distribution {} -- exit", quote_arg(distro))
    };
    run_wsl(&args)
}

/// Wraps `s` in double quotes if it contains whitespace, for building a
/// `wsl.exe` command line. Distribution names cannot themselves contain
/// quotes, so no further escaping is needed. Also used by `main` to build
/// the equivalent command line for the Terminal menu command.
pub fn quote_arg(s: &str) -> String {
    if s.contains(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// `768 MB` below 1 GiB, otherwise `5.40 GB` (binary units, two decimals).
pub fn format_bytes(b: u64) -> String {
    const MB: u64 = 1 << 20;
    if b < 1024 * MB {
        format!("{} MB", b / MB)
    } else {
        format!("{:.2} GB", b as f64 / (1u64 << 30) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(600 << 20), "600 MB");
        assert_eq!(format_bytes(1024 << 20), "1.00 GB");
        assert_eq!(format_bytes((5 << 30) + (1 << 29)), "5.50 GB");
    }

    #[test]
    fn quotes_args_only_when_needed() {
        assert_eq!(quote_arg("Ubuntu"), "Ubuntu");
        assert_eq!(quote_arg("Ubuntu 22.04"), "\"Ubuntu 22.04\"");
        assert_eq!(quote_arg(""), "");
    }

    #[test]
    fn utf16_compare() {
        let u16s = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert!(eq_lowercase_utf16(&u16s("vmmemWSL"), "vmmemwsl"));
        assert!(!eq_lowercase_utf16(&u16s("vmmem"), "vmmemwsl"));
        assert!(!eq_lowercase_utf16(&u16s("vmmemWSLx"), "vmmemwsl"));
        // Non-ASCII letters fold too, as with Go's strings.ToLower.
        assert!(eq_lowercase_utf16(&u16s("ÄBC.exe"), "äbc.exe"));
        assert!(eq_lowercase_utf16(&u16s("ZOË.EXE"), "zoë.exe"));
        assert!(!eq_lowercase_utf16(&u16s("ABC.exe"), "äbc.exe"));
    }

    /// Exercises the live sampler against whatever is running on this machine.
    /// Uses an always-present session-0 process when the WSL2 VM is not
    /// running (`find` only ever sees session 0, so a user-session process
    /// such as `explorer.exe` would never be found).
    #[test]
    fn poll_live() {
        for name in ["vmmemWSL", "services.exe"] {
            let mut m = Monitor::new(name);
            let (st, changed) = m.poll();
            eprintln!("{name}: first poll -> {st:?} changed={changed}");
            if !st.running {
                continue;
            }
            assert!(st.cpu.is_none(), "CPU must be unknown after baseline");
            std::thread::sleep(Duration::from_millis(1500));
            let (st, changed) = m.poll();
            eprintln!("{name}: second poll -> {st:?} changed={changed}");
            assert!(
                st.cpu.is_some(),
                "expected a CPU measurement on the next poll"
            );
            assert!(changed);
        }
    }

    /// A snapshot that starts with a buffer too small to hold the session-0
    /// process list grows it and succeeds; the new size is kept so later
    /// calls do not need to grow again.
    #[test]
    fn snapshot_grows() {
        let mut m = Monitor::new("services.exe");
        m.snapshot_size = 64;
        assert!(m.find().is_some());
        assert!(m.snapshot_size > 64);
        let grown = m.snapshot_size;
        assert!(m.find().is_some());
        assert_eq!(m.snapshot_size, grown, "no regrowth once it fits");
    }

    /// Cost of one presence check (the work done every `-poll` seconds).
    /// Run with `cargo test --release -- --ignored --nocapture poll_cost`.
    #[test]
    #[ignore]
    fn poll_cost() {
        let mut m = Monitor::new("vmmemWSL");
        m.poll(); // warm up: sizes the buffer
        let n = 500;
        let t = Instant::now();
        for _ in 0..n {
            m.find();
        }
        let per = t.elapsed() / n;
        eprintln!(
            "poll_cost: {} process-list snapshots, {:?} each, buffer {} KB",
            n,
            per,
            m.snapshot_size / 1024
        );
    }
}
