//! Windows: LocalSystem service that keeps the agent in the console
//! session (FR-32), performs Ctrl+Alt+Del (FR-14) and reboots (FR-34).

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ::windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use ::windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use ::windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    CryptUnprotectData,
};
use ::windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LUID_AND_ATTRIBUTES,
    LookupPrivilegeValueW, PSECURITY_DESCRIPTOR, SE_PRIVILEGE_ENABLED, SECURITY_ATTRIBUTES,
    SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation, TokenPrimary, TokenSessionId,
};
use ::windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
};
use ::windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use ::windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSQuerySessionInformationW, WTSUserName,
};
use ::windows::Win32::System::Shutdown::{
    InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
};
use ::windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
    GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
};
use ::windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CLEANBOOT};
use ::windows::core::{PCWSTR, PWSTR, w};

use crate::{SERVICE_NAME, ServiceError, Status, agent_args};

const SAS_PIPE: PCWSTR = w!(r"\\.\pipe\relay-sas");
/// Only SYSTEM may connect (the agent runs as SYSTEM).
const SAS_PIPE_SDDL: PCWSTR = w!("D:(A;;GA;;;SY)");
const STILL_ACTIVE: u32 = 259;

fn plat(e: impl std::fmt::Display) -> ServiceError {
    ServiceError::Platform(e.to_string())
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

// ------------------------------------------------------------ DPAPI

fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: b.len() as u32,
        pbData: b.as_ptr().cast_mut(),
    }
}

/// Encrypt at rest with machine-scope DPAPI (readable by SYSTEM and
/// administrators on this machine only).
pub fn dpapi_seal(plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input blob outlives the call; output freed with LocalFree.
    unsafe {
        CryptProtectData(
            &blob(plain),
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| e.to_string())?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Ok(v)
    }
}

pub fn dpapi_open(sealed: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as above.
    unsafe {
        CryptUnprotectData(
            &blob(sealed),
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| e.to_string())?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Ok(v)
    }
}

// ------------------------------------------------------------ queries

pub fn is_elevated() -> bool {
    // SAFETY: token query on our own process; handle closed.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut el = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut el).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && el.TokenIsElevated != 0
    }
}

/// User name logged in at the console, if any (FR-33 banner rule).
pub fn console_user() -> Option<String> {
    // SAFETY: WTS query with the documented free.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            return None;
        }
        let mut buf = PWSTR::null();
        let mut bytes = 0u32;
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSUserName,
            &mut buf,
            &mut bytes,
        )
        .ok()?;
        let name = buf.to_string().unwrap_or_default();
        WTSFreeMemory(buf.0.cast());
        (!name.is_empty()).then_some(name)
    }
}

// ------------------------------------------------------------ agent launch

/// A running agent process.
struct Agent {
    session: u32,
    process: HANDLE,
}

impl Agent {
    fn alive(&self) -> bool {
        let mut code = 0u32;
        // SAFETY: valid process handle owned by us.
        unsafe { GetExitCodeProcess(self.process, &mut code).is_ok() && code == STILL_ACTIVE }
    }

    fn stop(self) {
        // SAFETY: as above; the handle is closed exactly once here.
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Start `bin host --unattended` as SYSTEM inside console `session`.
fn launch_agent(bin: &Path, session: u32) -> Result<Agent, ServiceError> {
    let mut cmd: OsString = format!("\"{}\"", bin.display()).into();
    for a in agent_args() {
        cmd.push(" ");
        cmd.push(a);
    }
    let mut cmdline = wide(&cmd);
    let mut desktop = wide(OsStr::new(r"winsta0\default"));
    // SAFETY: Win32 token and process creation with owned handles; all
    // buffers outlive the calls.
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).map_err(plat)?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(
            own,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        let _ = CloseHandle(own);
        dup.map_err(plat)?;
        let mut sid = session;
        SetTokenInformation(token, TokenSessionId, (&raw mut sid).cast(), 4).map_err(plat)?;
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = CloseHandle(token);
        r.map_err(plat)?;
        let _ = CloseHandle(pi.hThread);
        Ok(Agent {
            session,
            process: pi.hProcess,
        })
    }
}

// ------------------------------------------------------------ SAS pipe

/// Serve Ctrl+Alt+Del requests from the agent (SYSTEM only).
fn sas_server(stop: &'static AtomicBool) {
    #[link(name = "sas")]
    unsafe extern "system" {
        fn SendSAS(as_user: i32);
    }
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: pipe created with a SYSTEM-only DACL; handle closed below.
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                SAS_PIPE_SDDL,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .is_err()
            {
                return;
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let pipe = CreateNamedPipeW(
                SAS_PIPE,
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                16,
                0,
                Some(&sa),
            );
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            if pipe.is_invalid() {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            if ConnectNamedPipe(pipe, None).is_ok() {
                let mut b = [0u8; 1];
                let mut n = 0u32;
                if ReadFile(pipe, Some(&mut b), Some(&mut n), None).is_ok()
                    && n == 1
                    && b[0] == b's'
                {
                    // Needs the SoftwareSASGeneration policy (set at install).
                    SendSAS(0);
                }
            }
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
    }
}

/// Agent side: ask the service for Ctrl+Alt+Del.
pub fn request_sas() -> Result<(), ServiceError> {
    // SAFETY: client end of the pipe; handle closed.
    unsafe {
        let h = CreateFileW(
            SAS_PIPE,
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| ServiceError::Platform(format!("Relay service not reachable: {e}")))?;
        let mut n = 0u32;
        let r = WriteFile(h, Some(b"s"), Some(&mut n), None);
        let _ = CloseHandle(h);
        r.map_err(plat)
    }
}

// ------------------------------------------------------------ service

static BIN: OnceLock<PathBuf> = OnceLock::new();
static STOP: AtomicBool = AtomicBool::new(false);

windows_service::define_windows_service!(ffi_service_main, service_main);

pub fn run_service(bin: &Path) -> Result<(), ServiceError> {
    let _ = BIN.set(bin.to_path_buf());
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(plat)
}

fn service_main(_args: Vec<OsString>) {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    let handler = move |ctl| match ctl {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            STOP.store(true, Ordering::Relaxed);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else {
        return;
    };
    let set = |state, accept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        });
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    );

    // Booted into Safe Mode by us (FR-34)? Make the next boot normal again.
    // SAFETY: plain getter.
    if unsafe { GetSystemMetrics(SM_CLEANBOOT) } != 0 {
        let _ = std::process::Command::new("bcdedit")
            .args(["/deletevalue", "{current}", "safeboot"])
            .status();
    }

    std::thread::spawn(|| sas_server(&STOP));
    let bin = BIN.get().cloned().unwrap_or_default();
    let mut agent: Option<Agent> = None;
    while !STOP.load(Ordering::Relaxed) {
        // SAFETY: plain getter.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let ok = agent
            .as_ref()
            .is_some_and(|a| a.session == session && a.alive());
        if !ok {
            if let Some(a) = agent.take() {
                a.stop();
            }
            if session != u32::MAX {
                match launch_agent(&bin, session) {
                    Ok(a) => agent = Some(a),
                    Err(e) => tracing::error!("cannot start agent in session {session}: {e}"),
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if let Some(a) = agent.take() {
        a.stop();
    }
    set(ServiceState::Stopped, ServiceControlAccept::empty());
}

// ------------------------------------------------------------ install

const SAFEBOOT_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\SafeBoot\Network\RelayService";
const SAS_POLICY_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";

fn reg(args: &[&str]) -> Result<(), ServiceError> {
    let st = std::process::Command::new("reg").args(args).status()?;
    if st.success() {
        Ok(())
    } else {
        Err(ServiceError::Platform(format!("reg {}", args.join(" "))))
    }
}

pub fn plan(bin: &Path) -> Vec<String> {
    vec![
        format!(
            "create service {SERVICE_NAME} (LocalSystem, automatic): \"{}\" service run",
            bin.display()
        ),
        format!("add {SAFEBOOT_KEY} so the service also runs in Safe Mode with networking"),
        format!(
            "set {SAS_POLICY_KEY}\\SoftwareSASGeneration = 1 (allow Ctrl+Alt+Del from services)"
        ),
        format!("start {SERVICE_NAME}"),
    ]
}

pub fn install(bin: &Path) -> Result<(), ServiceError> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let mgr = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(plat)?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Relay remote access".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: bin.to_path_buf(),
        launch_arguments: vec!["service".into(), "run".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = mgr
        .create_service(&info, ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .map_err(plat)?;
    reg(&["add", SAFEBOOT_KEY, "/ve", "/d", "Service", "/f"])?;
    reg(&[
        "add",
        SAS_POLICY_KEY,
        "/v",
        "SoftwareSASGeneration",
        "/t",
        "REG_DWORD",
        "/d",
        "1",
        "/f",
    ])?;
    svc.start::<&str>(&[]).map_err(plat)?;
    Ok(())
}

pub fn uninstall() -> Result<(), ServiceError> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(plat)?;
    if let Ok(svc) = mgr.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        let _ = svc.stop();
        svc.delete().map_err(plat)?;
    }
    let _ = reg(&["delete", SAFEBOOT_KEY, "/f"]);
    Ok(())
}

pub fn status() -> Status {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let svc = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .ok()
        .and_then(|m| {
            m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
                .ok()
        });
    match svc {
        None => Status {
            installed: false,
            running: false,
            detail: "not installed".into(),
        },
        Some(s) => {
            let running = s
                .query_status()
                .is_ok_and(|st| st.current_state == ServiceState::Running);
            Status {
                installed: true,
                running,
                detail: SERVICE_NAME.into(),
            }
        }
    }
}

// ------------------------------------------------------------ reboot

fn enable_shutdown_privilege() -> Result<(), ServiceError> {
    // SAFETY: adjusts our own token; handle closed.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token).map_err(plat)?;
        let mut tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: Default::default(),
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let r = LookupPrivilegeValueW(
            PCWSTR::null(),
            w!("SeShutdownPrivilege"),
            &mut tp.Privileges[0].Luid,
        )
        .and_then(|()| AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None));
        let _ = CloseHandle(token);
        r.map_err(plat)
    }
}

pub fn reboot(safe_mode: bool) -> Result<(), ServiceError> {
    if safe_mode {
        let st = std::process::Command::new("bcdedit")
            .args(["/set", "{current}", "safeboot", "network"])
            .status()?;
        if !st.success() {
            return Err(ServiceError::Platform("bcdedit failed".into()));
        }
    }
    enable_shutdown_privilege()?;
    // SAFETY: documented call; no buffers.
    unsafe {
        InitiateSystemShutdownExW(
            PCWSTR::null(),
            w!("Relay: remote restart"),
            0,
            true,
            true,
            SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
        )
        .map_err(plat)
    }
}
