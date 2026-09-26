//! Single-instance coordination over a loopback socket.
//!
//! A later launch closes the running ZapExt and takes its place. A current
//! build is asked to quit. An older build, which only understands show, is
//! stopped when the listening program file is one of ours. The operating
//! system releases the loopback port when that process ends.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fixed high loopback port outside the ephemeral range.
const INSTANCE_PORT: u16 = 47_119;

/// Stable wire identity shared with FastsApp so upgrades surface a running
/// older copy before migrating its session files.
const PREFIX: &str = "fastsapp:";
const OK_REPLY: &str = "fastsapp:ok";

pub enum Outcome {
    /// This process owns the instance guard.
    Only(Guard),
    /// The existing instance was asked to show its window.
    Surfaced,
}

/// Request from another launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    /// Shows or creates the window.
    Show,
    /// Reload local theme files without opening the window.
    ReloadThemes,
    /// Exits so the launch that asked can take the instance port.
    Quit,
}

/// Owns the listener that marks this process as the running instance.
pub struct Guard {
    /// Requests queued by later launches.
    commands: Arc<Mutex<Vec<ControlCommand>>>,
}

impl Guard {
    /// Shared request queue drained by the app.
    pub fn commands(&self) -> Arc<Mutex<Vec<ControlCommand>>> {
        Arc::clone(&self.commands)
    }
}

/// Sends one request and verifies the ZapFast reply prefix.
pub fn send(verb: &str) -> std::io::Result<()> {
    send_to(INSTANCE_PORT, verb)
}

fn send_to(port: u16, verb: &str) -> std::io::Result<()> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("{PREFIX}{verb}\n").as_bytes())?;
    // Read the one-line reply until the connection closes.
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    if reply.lines().next() == Some(OK_REPLY) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the port is held by something other than ZapFast",
        ))
    }
}

pub fn acquire(waker: &crate::backend::Waker) -> Outcome {
    let listener = match bind_instance(INSTANCE_PORT) {
        Ok(listener) => listener,
        Err(_) => {
            // Replacement failed. Surface a ZapFast holder, otherwise run anyway.
            if send("show").is_ok() {
                return Outcome::Surfaced;
            }
            log::warn!("port {INSTANCE_PORT} is busy but not with ZapFast; running unguarded");
            return Outcome::Only(Guard {
                commands: Default::default(),
            });
        }
    };
    Outcome::Only(listen(listener, waker))
}

/// Binds the instance port, asking the current holder to exit first.
fn bind_instance(port: u16) -> std::io::Result<TcpListener> {
    if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        return Ok(listener);
    }
    // A current build replies and quits. An older build ignores `quit`.
    let acknowledged = send_to(port, "quit").is_ok();
    if acknowledged && let Some(listener) = wait_for_bind(port, Duration::from_secs(2)) {
        return Ok(listener);
    }
    if let Some(pid) = listener_pid(port)
        && pid != std::process::id()
        && is_our_process(pid)
        && stop_process(pid)
        && let Some(listener) = wait_for_bind(port, Duration::from_secs(3))
    {
        log::info!("closed the previous ZapExt process {pid}");
        return Ok(listener);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "the running ZapExt did not release its port",
    ))
}

fn wait_for_bind(port: u16, budget: Duration) -> Option<TcpListener> {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            return Some(listener);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn listen(listener: TcpListener, waker: &crate::backend::Waker) -> Guard {
    let guard = Guard {
        commands: Default::default(),
    };
    let commands = Arc::clone(&guard.commands);
    let waker = waker.clone();
    let spawned = std::thread::Builder::new()
        .name("zapfast-instance".to_owned())
        .spawn(move || serve(listener, &commands, &waker));
    if let Err(error) = spawned {
        log::warn!("cannot listen for other launches: {error}");
    }
    guard
}

/// Handles one request and reply per connection until the listener closes.
fn serve(
    listener: TcpListener,
    commands: &Mutex<Vec<ControlCommand>>,
    waker: &crate::backend::Waker,
) {
    for mut stream in listener.incoming().flatten() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let Some(line) = read_line(&mut stream) else {
            continue;
        };
        // Ignore clients without the ZapFast prefix.
        if let Some(command) = parse(&line) {
            let _ = stream.write_all(format!("{OK_REPLY}\n").as_bytes());
            commands
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(command);
            waker.wake();
        }
    }
}

fn parse(line: &str) -> Option<ControlCommand> {
    match line.trim_end().strip_prefix(PREFIX)? {
        "show" => Some(ControlCommand::Show),
        "reload-themes" => Some(ControlCommand::ReloadThemes),
        "quit" => Some(ControlCommand::Quit),
        _ => None,
    }
}

/// Reads a bounded line and rejects read errors or oversized input.
fn read_line(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = [0u8; 256];
    let mut filled = 0;
    loop {
        if filled == buffer.len() {
            return None;
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if buffer[..filled].contains(&b'\n') {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let line = buffer[..filled].split(|&byte| byte == b'\n').next()?;
    String::from_utf8(line.to_vec()).ok()
}

/// File names that may hold the instance port across the rename history.
fn is_instance_file_name(file_name: &std::ffi::OsStr) -> bool {
    let name = file_name.to_string_lossy().to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    let (base, suffix) = stem
        .split_once('-')
        .map(|(base, suffix)| (base, Some(suffix)))
        .unwrap_or((stem, None));
    if !matches!(base, "zapext" | "zapfast" | "fastsapp") {
        return false;
    }
    // `zapext-1.0.95.exe` is a copy of this program. A test binary such as
    // `zapfast-<hash>.exe` is not.
    suffix.is_none_or(is_version_suffix)
}

fn is_version_suffix(suffix: &str) -> bool {
    let suffix = suffix.strip_prefix('v').unwrap_or(suffix);
    let mut parts = suffix.split('.');
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    parts.next().is_some_and(numeric)
        && parts.next().is_some_and(numeric)
        && parts.next().is_some_and(numeric)
        && parts.next().is_none()
}

fn is_our_process(pid: u32) -> bool {
    process_file_name(pid).is_some_and(|name| is_instance_file_name(&name))
}

/// Process listening on `port`, when the platform can name the owner.
fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    {
        windows_listener_pid(port)
    }
    #[cfg(target_os = "linux")]
    {
        linux_listener_pid(port)
    }
    #[cfg(target_os = "macos")]
    {
        macos_listener_pid(port)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = port;
        None
    }
}

#[cfg(windows)]
fn windows_listener_pid(port: u16) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };

    unsafe {
        let mut size = 0u32;
        let first = GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if first != ERROR_INSUFFICIENT_BUFFER || size == 0 {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        let second = GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &mut size,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if second != 0 {
            return None;
        }
        let table = &*(buffer.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>());
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        rows.iter().find_map(|row| {
            let found = u16::from_be((row.dwLocalPort & 0xffff) as u16);
            (found == port).then_some(row.dwOwningPid)
        })
    }
}

#[cfg(target_os = "linux")]
fn linux_listener_pid(port: u16) -> Option<u32> {
    let table = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let inode = proc_tcp_listen_inode(&table, port)?;
    scan_proc_for_socket(std::path::Path::new("/proc"), inode)
}

/// The pid in `proc` holding `socket:[inode]` open, if any.
///
/// Non-numeric entries (`net`, `sys`, ...) and unreadable fd directories
/// (other users, exited processes) are skipped: aborting on them would
/// never reach the replace fallback that runs after this scan.
#[cfg(target_os = "linux")]
fn scan_proc_for_socket(proc: &std::path::Path, inode: u64) -> Option<u32> {
    let needle = format!("socket:[{inode}]");
    for entry in std::fs::read_dir(proc).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(proc.join(pid.to_string()).join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path())
                .ok()
                .is_some_and(|target| target.to_string_lossy() == needle)
            {
                return Some(pid);
            }
        }
    }
    None
}

/// Inode of the IPv4 listener for `port` in a `/proc/net/tcp` table.
///
/// Pure parser over table text. Production needs it on Linux only; tests
/// keep it compiled everywhere so a miscount can never hide behind a
/// platform gate again.
#[cfg(any(target_os = "linux", test))]
fn proc_tcp_listen_inode(table: &str, port: u16) -> Option<u64> {
    for line in table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _slot = fields.next()?;
        let local = fields.next()?;
        let _remote = fields.next()?;
        let state = fields.next()?;
        // 0A is TCP_LISTEN.
        if state != "0A" {
            continue;
        }
        let hex_port = local.rsplit(':').next()?;
        if u16::from_str_radix(hex_port, 16).ok()? != port {
            continue;
        }
        // After sl/local/rem/st, each of these is ONE whitespace token:
        // tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout, inode.
        // The inode is the 6th remaining token, hence nth(5).
        return fields.nth(5)?.parse().ok();
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_listener_pid(port: u16) -> Option<u32> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Basename of a reported process name, whether the system gave a bare
/// name or a full path (`ps` differs by platform and flags). Production
/// needs it on macOS only; tests keep it compiled everywhere so the
/// contract is verified on every platform's CI.
#[cfg(any(target_os = "macos", test))]
fn process_name_basename(raw: &str) -> Option<std::ffi::OsString> {
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    Some(
        std::path::Path::new(name)
            .file_name()
            .map(|base| base.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from(name)),
    )
}

fn process_file_name(pid: u32) -> Option<std::ffi::OsString> {
    #[cfg(windows)]
    {
        windows_process_file_name(pid)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()?
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        let name = String::from_utf8(output.stdout).ok()?;
        process_name_basename(&name)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(windows)]
fn windows_process_file_name(pid: u32) -> Option<std::ffi::OsString> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let text = String::from_utf16_lossy(&buffer[..length as usize]);
        std::path::Path::new(&text)
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
    }
}

fn stop_process(pid: u32) -> bool {
    #[cfg(windows)]
    {
        windows_stop_process(pid)
    }
    #[cfg(unix)]
    {
        unsafe { libc_kill(pid as i32, 15) == 0 }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}

#[cfg(windows)]
fn windows_stop_process(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let stopped = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_own_show_is_understood() {
        assert_eq!(parse("fastsapp:show\n"), Some(ControlCommand::Show));
        assert_eq!(parse("fastsapp:show"), Some(ControlCommand::Show));
        assert_eq!(parse("fastsapp:quit\n"), Some(ControlCommand::Quit));
        assert_eq!(parse("GET / HTTP/1.1"), None);
        assert_eq!(parse("fastsapp:frobnicate"), None);
        assert_eq!(parse(""), None);
    }

    /// Verifies a request crosses the socket into the app queue.
    #[test]
    fn a_second_launch_reaches_the_queue() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let commands: Arc<Mutex<Vec<ControlCommand>>> = Default::default();
        let served = {
            let commands = Arc::clone(&commands);
            let waker = crate::backend::Waker::default();
            std::thread::spawn(move || serve(listener, &commands, &waker))
        };

        send_to(port, "show").expect("answered as ZapFast");
        // Unknown verbs close the connection without a reply.
        assert!(send_to(port, "frobnicate").is_err());

        assert_eq!(
            *commands.lock().expect("the queue"),
            vec![ControlCommand::Show]
        );
        drop(served);
    }

    #[test]
    fn instance_names_cover_the_rename_history() {
        use std::ffi::OsStr;
        assert!(is_instance_file_name(OsStr::new("zapext.exe")));
        assert!(is_instance_file_name(OsStr::new("ZapFast.EXE")));
        assert!(is_instance_file_name(OsStr::new("fastsapp")));
        assert!(is_instance_file_name(OsStr::new("zapext-1.0.95.exe")));
        assert!(is_instance_file_name(OsStr::new("zapext-1.0.96.exe")));
        assert!(!is_instance_file_name(OsStr::new(
            "zapfast-7904565fa5219df1.exe"
        )));
        assert!(!is_instance_file_name(OsStr::new("notepad.exe")));
    }

    #[test]
    fn the_listener_pid_is_this_process() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        assert_eq!(listener_pid(port), Some(std::process::id()));
        drop(listener);
    }

    #[test]
    fn quit_releases_the_port_for_the_next_launch() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("a bound address").port();
        let served = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            if read_line(&mut stream).as_deref() == Some("fastsapp:quit") {
                let _ = stream.write_all(b"fastsapp:ok\n");
            }
            drop(listener);
        });
        let claimed = bind_instance(port).expect("the quitter released the port");
        drop(claimed);
        served.join().expect("server");
    }

    /// Pure parser: runs on every platform, including Windows CI, so a
    /// miscount can never hide behind a platform gate again.
    #[test]
    fn proc_tcp_inode_reads_a_listener() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:B80F 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(proc_tcp_listen_inode(table, 47_119), Some(4242));
        assert_eq!(proc_tcp_listen_inode(table, 1), None);
    }

    #[test]
    fn process_names_keep_only_the_basename() {
        use std::ffi::OsString;
        assert_eq!(
            process_name_basename("zapext"),
            Some(OsString::from("zapext"))
        );
        assert_eq!(
            process_name_basename("/Applications/ZapExt.app/Contents/MacOS/zapext"),
            Some(OsString::from("zapext"))
        );
        assert_eq!(process_name_basename(""), None);
        assert_eq!(process_name_basename("   \n"), None);
    }

    /// A fake /proc: non-numeric entries, a pid without an fd directory,
    /// and the pid holding our socket. Every entry must be skipped or
    /// matched on its own; aborting on junk would never reach the match.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_scan_skips_junk_and_finds_the_socket() {
        use std::os::unix::fs::symlink;
        let base = tempfile::tempdir().expect("scratch proc tree");
        std::fs::create_dir(base.path().join("net")).unwrap();
        std::fs::create_dir_all(base.path().join("111").join("unrelated")).unwrap();
        let fds = base.path().join("222").join("fd");
        std::fs::create_dir_all(&fds).unwrap();
        symlink("socket:[999]", fds.join("0")).unwrap();
        symlink("socket:[4242]", fds.join("7")).unwrap();
        assert_eq!(scan_proc_for_socket(base.path(), 4242), Some(222));
        assert_eq!(scan_proc_for_socket(base.path(), 1), None);
    }
}
