//! The deep-mode helper (SPEC §13.4) and who may run which ETW session.
//!
//! - Present events: a real-time session in this process when its token is an elevated admin's or
//!   holds Performance Log Users. `strobe up` adds the user to that group (it counts from the next
//!   logon); until then, and for anything the group cannot do, the helper runs the session.
//! - Sampled stacks: SeSystemProfilePrivilege, administrators only: always the helper.
//!
//! The helper is `strobe helper serve`, run by a scheduled task with the user's highest privileges
//! from a copy in `%ProgramFiles%\strobe\` (only an elevated step writes there: `strobe up`). The
//! user's strobe starts the task on demand (`schtasks /run`, no prompt), talks to it over
//! `\\.\pipe\strobe-helper-<SID>` (ACL: that user, SYSTEM and administrators; local clients only)
//! and the helper does a fixed set of operations: start and stop a present session, start and stop
//! a sampled-stack session for a process tree the user runs, returning the events over the pipe.
//! It exits after two minutes without a client. One UAC prompt (in `up`) installs it.

use super::Access;
use crate::model::{Error, Result};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::{PSID, SECURITY_ATTRIBUTES};

const PERF_LOG_USERS: &str = "S-1-5-32-559";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";
const IDLE_EXIT: Duration = Duration::from_secs(120);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A SID from its string form; freed on drop.
struct Sid(PSID);

impl Sid {
    fn parse(s: &str) -> Option<Sid> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        let w = wide(s);
        let mut p: PSID = std::ptr::null_mut();
        (unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) } != 0).then_some(Sid(p))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0 as _) };
    }
}

/// Whether this process's token has `sid` enabled (deny-only groups of a filtered admin token do not count).
fn token_has(sid: &str) -> bool {
    use windows_sys::Win32::Security::CheckTokenMembership;
    let Some(s) = Sid::parse(sid) else { return false };
    let mut member = 0;
    (unsafe { CheckTokenMembership(std::ptr::null_mut(), s.0, &mut member) }) != 0 && member != 0
}

/// The SID of the user this process runs as.
pub fn user_sid() -> Option<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut len) != 0;
        CloseHandle(token);
        if !ok {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return None;
        }
        let mut n = 0;
        while *text.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
        LocalFree(text as _);
        Some(s)
    }
}

/// The owner SID of a kernel object handle (opened with READ_CONTROL, as GENERIC_READ includes).
fn pipe_owner(h: HANDLE) -> Option<String> {
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    unsafe {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if GetSecurityInfo(h, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd) != 0 || owner.is_null() {
            return None;
        }
        let mut text: *mut u16 = std::ptr::null_mut();
        let out = (ConvertSidToStringSidW(owner, &mut text) != 0).then(|| {
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text as _);
            s
        });
        LocalFree(sd as _);
        out
    }
}

/// The localized name of a well-known group (`Performance Log Users` in English).
fn account_name(sid: &str) -> Option<String> {
    use windows_sys::Win32::Security::LookupAccountSidW;
    let s = Sid::parse(sid)?;
    let (mut name, mut domain) = (vec![0u16; 256], vec![0u16; 256]);
    let (mut n, mut d, mut kind) = (256u32, 256u32, 0);
    if unsafe { LookupAccountSidW(std::ptr::null(), s.0, name.as_mut_ptr(), &mut n, domain.as_mut_ptr(), &mut d, &mut kind) } == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&name[..n as usize]))
}

/// Whether `user` is a member of Performance Log Users (from the group, not this logon's token).
fn in_group(user: &str) -> bool {
    use windows_sys::Win32::NetworkManagement::NetManagement::{LOCALGROUP_MEMBERS_INFO_0, NetApiBufferFree, NetLocalGroupGetMembers};
    use windows_sys::Win32::Security::EqualSid;
    let (Some(group), Some(me)) = (account_name(PERF_LOG_USERS), Sid::parse(user)) else { return false };
    let g = wide(&group);
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total, mut resume) = (0u32, 0u32, 0usize);
    if unsafe { NetLocalGroupGetMembers(std::ptr::null(), g.as_ptr(), 0, &mut buf, u32::MAX, &mut read, &mut total, &mut resume) } != 0 {
        return false;
    }
    let members = unsafe { std::slice::from_raw_parts(buf as *const LOCALGROUP_MEMBERS_INFO_0, read as usize) };
    let found = members.iter().any(|m| unsafe { EqualSid(m.lgrmi0_sid, me.0) } != 0);
    unsafe { NetApiBufferFree(buf as _) };
    found
}

fn program_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramW6432").or_else(|_| std::env::var("ProgramFiles")).unwrap_or_else(|_| "C:\\Program Files".into())).join("strobe")
}

pub fn helper_exe() -> PathBuf {
    program_dir().join("strobe-helper.exe")
}

/// `strobe\helper-<RID>`: one task per user.
pub fn task_name(sid: &str) -> String {
    format!("strobe\\helper-{}", sid.rsplit('-').next().unwrap_or("0"))
}

fn pipe_name(sid: &str) -> String {
    format!(r"\\.\pipe\strobe-helper-{sid}")
}

pub fn access() -> Access {
    let token = if token_has(ADMINISTRATORS) {
        "admin"
    } else if token_has(PERF_LOG_USERS) {
        "group"
    } else {
        "none"
    };
    let sid = user_sid().unwrap_or_default();
    let pending_sign_out = token == "none" && in_group(&sid);
    Access { token, pending_sign_out, helper: helper_exe().is_file() && task_exists(&sid) }
}

fn task_exists(sid: &str) -> bool {
    crate::host::output(std::process::Command::new("schtasks").args(["/query", "/tn", &task_name(sid)]).creation_flags_hidden()).is_some()
}

trait Hidden {
    fn creation_flags_hidden(&mut self) -> &mut Self;
}

impl Hidden for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null())
    }
}

/// The binary the helper runs: the windowless build beside this one, so the task opens no console.
fn source_exe() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let w = me.with_file_name("strobew.exe");
    Some(if w.is_file() { w } else { me })
}

/// Runs this binary elevated with `args` (one UAC prompt) and waits; returns its exit code.
pub fn elevated(args: &str) -> Result<u32> {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    let exe = std::env::current_exe()?;
    let (file, params, verb) = (wide(&exe.display().to_string()), wide(args), wide("runas"));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let rc = unsafe { GetLastError() };
        return Err(if rc == 1223 { Error::msg("the UAC prompt was declined") } else { Error::msg(format!("elevate: error {rc}")) });
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    Ok(code)
}

/// `strobe helper install --sid <SID>` (elevated): Performance Log Users, the helper copy, the task.
pub fn install(sid: &str) -> Result<String> {
    if !token_has(ADMINISTRATORS) {
        return Err(Error::msg("helper install needs an elevated token (strobe up asks for it)"));
    }
    let mut notes = Vec::new();
    notes.push(match add_to_group(sid) {
        Ok(true) => "added to Performance Log Users".to_string(),
        Ok(false) => "already in Performance Log Users".to_string(),
        Err(e) => format!("Performance Log Users: {}", e.message),
    });
    let dir = program_dir();
    std::fs::create_dir_all(&dir)?;
    let dst = helper_exe();
    let src = source_exe().ok_or_else(|| Error::msg("find this binary"))?;
    // A running helper locks its file: end it, then move the old copy aside.
    let _ = crate::host::output(std::process::Command::new("schtasks").args(["/end", "/tn", &task_name(sid)]).creation_flags_hidden());
    if dst.exists() {
        let old = dst.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&dst, &old);
    }
    std::fs::copy(&src, &dst)?;
    let xml = task_xml(sid, &dst);
    let file = std::env::temp_dir().join(format!("strobe-helper-{}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|c| c.to_le_bytes()));
    std::fs::write(&file, bytes)?;
    let ok = std::process::Command::new("schtasks")
        .args(["/create", "/tn", &task_name(sid), "/xml"])
        .arg(&file)
        .arg("/f")
        .creation_flags_hidden()
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&file);
    if !ok {
        return Err(Error::msg("schtasks /create failed"));
    }
    notes.push(format!("task {} runs {}", task_name(sid), dst.display()));
    Ok(notes.join(" · "))
}

/// `strobe helper uninstall --sid <SID>` (elevated): the task, the copy and the group membership.
pub fn uninstall(sid: &str) -> Result<String> {
    use windows_sys::Win32::NetworkManagement::NetManagement::{LOCALGROUP_MEMBERS_INFO_0, NetLocalGroupDelMembers};
    let name = task_name(sid);
    let _ = crate::host::output(std::process::Command::new("schtasks").args(["/end", "/tn", &name]).creation_flags_hidden());
    let _ = crate::host::output(std::process::Command::new("schtasks").args(["/delete", "/tn", &name, "/f"]).creation_flags_hidden());
    let _ = std::fs::remove_dir_all(program_dir());
    if let (Some(group), Some(user)) = (account_name(PERF_LOG_USERS), Sid::parse(sid)) {
        let g = wide(&group);
        let m = LOCALGROUP_MEMBERS_INFO_0 { lgrmi0_sid: user.0 };
        unsafe { NetLocalGroupDelMembers(std::ptr::null(), g.as_ptr(), 0, &m as *const _ as *const u8, 1) };
    }
    Ok(format!("removed task {name}, {} and Performance Log Users membership", program_dir().display()))
}

fn add_to_group(sid: &str) -> Result<bool> {
    use windows_sys::Win32::NetworkManagement::NetManagement::{LOCALGROUP_MEMBERS_INFO_0, NetLocalGroupAddMembers};
    let group = account_name(PERF_LOG_USERS).ok_or_else(|| Error::msg("look up Performance Log Users"))?;
    let user = Sid::parse(sid).ok_or_else(|| Error::msg(format!("bad SID {sid}")))?;
    let g = wide(&group);
    let m = LOCALGROUP_MEMBERS_INFO_0 { lgrmi0_sid: user.0 };
    match unsafe { NetLocalGroupAddMembers(std::ptr::null(), g.as_ptr(), 0, &m as *const _ as *const u8, 1) } {
        0 => Ok(true),
        1378 => Ok(false), // ERROR_MEMBER_IN_ALIAS
        rc => Err(Error::msg(format!("error {rc}"))),
    }
}

fn task_xml(sid: &str, exe: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>strobe deep-mode helper: ETW sessions (present events, sampled stacks) for this user's strobe, on request over a user-only pipe. Removed by strobe leave.</Description><SecurityDescriptor>D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;GRGX;;;{sid})</SecurityDescriptor></RegistrationInfo>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>helper serve --sid {sid}</Arguments></Exec></Actions>
</Task>
"#,
        exe.display()
    )
}

/// `strobe helper serve --sid <SID>`: the elevated side, until two minutes pass without a client.
pub fn serve(sid: &str, log: &mut dyn Write) -> Result<()> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT};
    if Sid::parse(sid).is_none() {
        return Err(Error::usage("helper serve --sid <SID>"));
    }
    // That user, SYSTEM and administrators; medium integrity may write (the user's strobe is not elevated).
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)"));
    let mut sd = std::ptr::null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, std::ptr::null_mut()) } == 0 {
        return Err(Error::msg("build the pipe's security descriptor"));
    }
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let name = wide(&pipe_name(sid));
    let h = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            1 << 16,
            1 << 16,
            0,
            &sa,
        )
    };
    unsafe { LocalFree(sd as _) };
    if h == INVALID_HANDLE_VALUE {
        return Err(Error::msg(format!("create the helper pipe: error {} (another helper runs?)", unsafe { GetLastError() })));
    }
    let _ = writeln!(log, "helper {} serving {}", std::process::id(), pipe_name(sid));
    let pipe = Pipe(h);
    loop {
        if !pipe.accept(IDLE_EXIT) {
            let _ = writeln!(log, "idle: exiting");
            break;
        }
        let mut conn = Conn::default();
        while let Some(line) = pipe.read_line() {
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(req) => conn.handle(&req, sid).unwrap_or_else(|e| json!({ "err": e.message })),
                Err(e) => json!({ "err": format!("bad request: {e}") }),
            };
            let _ = writeln!(log, "{} → {}", req_op(&line), if reply.get("err").is_some() { reply["err"].to_string() } else { "ok".into() });
            if !pipe.write_line(&reply.to_string()) {
                break;
            }
        }
        conn.close();
        unsafe { DisconnectNamedPipe(pipe.0) };
    }
    Ok(())
}

fn req_op(line: &str) -> String {
    serde_json::from_str::<Value>(line).ok().and_then(|v| v["op"].as_str().map(str::to_string)).unwrap_or_default()
}

/// One client's sessions; a client that goes away stops them.
#[derive(Default)]
struct Conn {
    presents: Option<super::present::Capture>,
    stacks: Option<super::stacks::Capture>,
}

impl Conn {
    fn handle(&mut self, req: &Value, sid: &str) -> Result<Value> {
        match req["op"].as_str().unwrap_or("") {
            "hello" => Ok(json!({ "ok": { "pid": std::process::id(), "build": env!("STROBE_BUILD_ID"), "protocol": env!("STROBE_HELPER_PROTOCOL") } })),
            "presents.start" => {
                if self.presents.is_none() {
                    self.presents = Some(super::present::Capture::start()?);
                }
                Ok(json!({ "ok": {} }))
            }
            "presents.live" => {
                let c = self.presents.as_ref().ok_or_else(|| Error::msg("no present session"))?;
                let pids: Vec<u32> = req["pids"].as_array().into_iter().flatten().filter_map(|p| p.as_u64().map(|p| p as u32)).collect();
                let wait = Duration::from_millis(req["wait_ms"].as_u64().unwrap_or(0).min(60_000));
                Ok(json!({ "ok": c.live.view(&pids, req["after_ns"].as_u64().unwrap_or(0), wait) }))
            }
            "presents.stop" => {
                let c = self.presents.take().ok_or_else(|| Error::msg("no present session"))?;
                Ok(json!({ "ok": c.stop()? }))
            }
            "stacks.start" => {
                let root = req["root"].as_u64().ok_or_else(|| Error::msg("stacks.start needs root"))? as u32;
                if !owned_by(root, sid) {
                    return Err(Error::msg(format!("process {root} does not run as this user")));
                }
                if self.stacks.is_none() {
                    self.stacks = Some(super::stacks::Capture::start(root)?);
                }
                Ok(json!({ "ok": {} }))
            }
            "stacks.stop" => {
                let c = self.stacks.take().ok_or_else(|| Error::msg("no stack session"))?;
                Ok(json!({ "ok": c.stop()? }))
            }
            other => Err(Error::msg(format!("unknown op {other}"))),
        }
    }

    fn close(&mut self) {
        if let Some(c) = self.presents.take() {
            let _ = c.stop();
        }
        if let Some(c) = self.stacks.take() {
            let _ = c.stop();
        }
    }
}

/// Whether a process runs as `sid` (the helper samples only the user's own processes).
fn owned_by(pid: u32, sid: &str) -> bool {
    use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
    let Some(want) = Sid::parse(sid) else { return false };
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return false;
        }
        let mut token = std::ptr::null_mut();
        let ok = OpenProcessToken(p, TOKEN_QUERY, &mut token) != 0;
        CloseHandle(p);
        if !ok {
            return false;
        }
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        let got = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut len) != 0;
        CloseHandle(token);
        got && EqualSid((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, want.0) != 0
    }
}

/// The server's end of the pipe, with overlapped I/O so waiting for a client can time out.
struct Pipe(HANDLE);

impl Pipe {
    fn wait(&self, ov: &mut windows_sys::Win32::System::IO::OVERLAPPED, started: i32, timeout: Option<Duration>) -> Option<u32> {
        use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        const IO_PENDING: u32 = 997;
        const PIPE_CONNECTED: u32 = 535;
        let mut n = 0u32;
        if started == 0 {
            match unsafe { GetLastError() } {
                IO_PENDING => {}
                PIPE_CONNECTED => return Some(0),
                _ => return None,
            }
            let ms = timeout.map_or(u32::MAX, |t| t.as_millis() as u32);
            if unsafe { WaitForSingleObject(ov.hEvent, ms) } != 0 {
                unsafe {
                    CancelIo(self.0);
                    GetOverlappedResult(self.0, ov, &mut n, 1);
                }
                return None;
            }
        }
        (unsafe { GetOverlappedResult(self.0, ov, &mut n, 1) } != 0).then_some(n)
    }

    fn overlapped() -> windows_sys::Win32::System::IO::OVERLAPPED {
        use windows_sys::Win32::System::Threading::CreateEventW;
        let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        ov
    }

    fn accept(&self, timeout: Duration) -> bool {
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        let mut ov = Self::overlapped();
        let started = unsafe { ConnectNamedPipe(self.0, &mut ov) };
        let ok = started != 0 || self.wait(&mut ov, started, Some(timeout)).is_some();
        unsafe { CloseHandle(ov.hEvent) };
        ok
    }

    fn read_line(&self) -> Option<String> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        let mut line = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut ov = Self::overlapped();
            let started = unsafe { ReadFile(self.0, buf.as_mut_ptr(), 1, std::ptr::null_mut(), &mut ov) };
            let n = self.wait(&mut ov, started, None);
            unsafe { CloseHandle(ov.hEvent) };
            match n {
                Some(1) if buf[0] == b'\n' => return String::from_utf8(line).ok(),
                Some(1) => line.push(buf[0]),
                _ => return None,
            }
            if line.len() > 1 << 20 {
                return None;
            }
        }
    }

    fn write_line(&self, text: &str) -> bool {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        let mut at = 0;
        while at < bytes.len() {
            let mut ov = Self::overlapped();
            let chunk = (bytes.len() - at).min(1 << 20);
            let started = unsafe { WriteFile(self.0, bytes[at..].as_ptr(), chunk as u32, std::ptr::null_mut(), &mut ov) };
            let n = self.wait(&mut ov, started, None);
            unsafe { CloseHandle(ov.hEvent) };
            match n {
                Some(n) if n > 0 => at += n as usize,
                _ => return false,
            }
        }
        true
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The user's side: a connection to the helper, started through its task when it is not running.
pub struct Client(HANDLE);

// The handle is used by one thread at a time (requests and replies alternate).
unsafe impl Send for Client {}

impl Client {
    pub fn connect() -> Result<Client> {
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
        use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
        let sid = user_sid().ok_or_else(|| Error::msg("read this user's SID"))?;
        let name = wide(&pipe_name(&sid));
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut started = false;
        loop {
            let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
            if h != INVALID_HANDLE_VALUE {
                // Only the installed helper may answer: the server runs the installed image (a medium
                // process may read an elevated one's image name), and the pipe belongs to this user or
                // to administrators or SYSTEM (an elevated token's default owner). Another account that
                // created a pipe of this name first owns it itself, even running a copy of the helper.
                let mut pid = 0u32;
                let served = unsafe { GetNamedPipeServerProcessId(h, &mut pid) } != 0 && pid != 0;
                let exe = served.then(|| crate::host::win::exe_of(pid)).flatten();
                let image = exe.as_ref().is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(&helper_exe().to_string_lossy()));
                let owner = pipe_owner(h);
                let owned = owner.as_deref().is_some_and(|o| o == sid || o == ADMINISTRATORS || o == SYSTEM);
                if image && owned {
                    return Client(h).hello();
                }
                unsafe { CloseHandle(h) };
                return Err(Error::msg(format!(
                    "refused the helper pipe: served by {} (pid {pid}, owner {}), not the installed helper {} run by this user or an administrator",
                    exe.map(|e| e.display().to_string()).unwrap_or_else(|| "an unreadable process".into()),
                    owner.unwrap_or_else(|| "unreadable".into()),
                    helper_exe().display()
                ))
                .hint("another account may have created the pipe first: sign it out, or run strobe up again"));
            }
            let rc = unsafe { GetLastError() };
            if rc == 231 {
                unsafe { WaitNamedPipeW(name.as_ptr(), 5000) };
            } else if !started {
                if !helper_exe().is_file() {
                    return Err(Error::msg("the deep-mode helper is not installed: run strobe up (one UAC prompt)"));
                }
                let ok = std::process::Command::new("schtasks").args(["/run", "/tn", &task_name(&sid)]).creation_flags_hidden().stdout(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
                if !ok {
                    return Err(Error::msg("start the helper task failed: run strobe up to reinstall it"));
                }
                started = true;
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::msg("the helper did not answer within 15 s"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Refuses a helper whose protocol is not this build's: `strobe update` (or another build) left an
    /// older copy installed, whose requests and replies may differ in what they mean.
    fn hello(mut self) -> Result<Client> {
        let hi = self.call(json!({ "op": "hello" }))?;
        let protocol = hi["protocol"].as_str().unwrap_or("");
        if protocol != env!("STROBE_HELPER_PROTOCOL") {
            return Err(Error::msg(format!(
                "the installed deep-mode helper is another build ({}, protocol {}; this build {}, protocol {})",
                hi["build"].as_str().or(hi["version"].as_str()).unwrap_or("unknown"),
                if protocol.is_empty() { "unknown" } else { protocol },
                env!("STROBE_BUILD_ID"),
                env!("STROBE_HELPER_PROTOCOL")
            ))
            .hint("run strobe up from this build to install its helper (one UAC prompt)"));
        }
        Ok(self)
    }

    pub fn call(&mut self, req: Value) -> Result<Value> {
        use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
        let mut bytes = req.to_string().into_bytes();
        bytes.push(b'\n');
        let mut n = 0u32;
        if unsafe { WriteFile(self.0, bytes.as_ptr(), bytes.len() as u32, &mut n, std::ptr::null_mut()) } == 0 {
            return Err(Error::msg("write to the helper"));
        }
        let mut reply = Vec::with_capacity(1 << 16);
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let mut got = 0u32;
            if unsafe { ReadFile(self.0, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null_mut()) } == 0 && got == 0 {
                return Err(Error::msg("the helper closed the pipe"));
            }
            reply.extend_from_slice(&buf[..got as usize]);
            if reply.last() == Some(&b'\n') {
                break;
            }
        }
        let v: Value = serde_json::from_slice(&reply).map_err(|e| Error::msg(format!("helper reply: {e}")))?;
        if let Some(e) = v.get("err") {
            return Err(Error::msg(format!("helper: {}", e.as_str().unwrap_or(""))));
        }
        Ok(v["ok"].clone())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
