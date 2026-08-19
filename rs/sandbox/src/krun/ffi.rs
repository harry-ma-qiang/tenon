use libloading::{Library, Symbol};
use std::ffi::c_char;

/// The C API this backend is written against: libkrun 1.9's `krun.h`
/// (<https://github.com/containers/libkrun>, `include/libkrun.h`), which is
/// ABI-stable since 1.0 — every symbol below has kept its signature across the
/// 1.x line. Nothing is linked at build time: the library is opened with
/// `dlopen` on first use, so a `tenon` binary built on a machine without
/// libkrun still runs everywhere, and the backend reports "not available"
/// instead of failing to start.
pub const API: &str = "libkrun 1.9 (krun.h, ABI 1.x)";

/// Tried in order. `TENON_LIBKRUN` overrides the whole list with one path.
#[cfg(target_os = "macos")]
pub const NAMES: &[&str] = &["libkrun.dylib", "libkrun.1.dylib"];
#[cfg(not(target_os = "macos"))]
pub const NAMES: &[&str] = &["libkrun.so.1", "libkrun.so"];

type SetLogLevel = unsafe extern "C" fn(u32) -> i32;
type CreateCtx = unsafe extern "C" fn() -> i32;
type FreeCtx = unsafe extern "C" fn(u32) -> i32;
type SetVmConfig = unsafe extern "C" fn(u32, u8, u32) -> i32;
type SetRoot = unsafe extern "C" fn(u32, *const c_char) -> i32;
type SetWorkdir = unsafe extern "C" fn(u32, *const c_char) -> i32;
type SetExec =
    unsafe extern "C" fn(u32, *const c_char, *const *const c_char, *const *const c_char) -> i32;
type SetEnv = unsafe extern "C" fn(u32, *const *const c_char) -> i32;
type AddVirtiofs = unsafe extern "C" fn(u32, *const c_char, *const c_char) -> i32;
type SetPortMap = unsafe extern "C" fn(u32, *const *const c_char) -> i32;
type AddVsockPort = unsafe extern "C" fn(u32, u32, *const c_char) -> i32;
type SetRlimits = unsafe extern "C" fn(u32, *const *const c_char) -> i32;
type SetConsoleOutput = unsafe extern "C" fn(u32, *const c_char) -> i32;
type StartEnter = unsafe extern "C" fn(u32) -> i32;

/// The resolved entry points. The `Library` is kept in the same struct and
/// never dropped while a pointer is held, which is what makes the copied
/// function pointers sound.
pub struct Api {
    _library: Library,
    pub set_log_level: SetLogLevel,
    pub create_ctx: CreateCtx,
    pub free_ctx: Option<FreeCtx>,
    pub set_vm_config: SetVmConfig,
    pub set_root: SetRoot,
    pub set_workdir: SetWorkdir,
    pub set_exec: SetExec,
    pub set_env: SetEnv,
    pub add_virtiofs: AddVirtiofs,
    pub set_port_map: Option<SetPortMap>,
    pub add_vsock_port: Option<AddVsockPort>,
    pub set_rlimits: Option<SetRlimits>,
    pub set_console_output: Option<SetConsoleOutput>,
    pub start_enter: StartEnter,
}

/// The symbols that must resolve, in the order they are looked up. A library
/// that is missing one of these is not a libkrun this backend can drive.
pub const REQUIRED: &[&str] = &[
    "krun_set_log_level",
    "krun_create_ctx",
    "krun_set_vm_config",
    "krun_set_root",
    "krun_set_workdir",
    "krun_set_exec",
    "krun_set_env",
    "krun_add_virtiofs",
    "krun_start_enter",
];

/// Optional symbols: present in every 1.x libkrun built with TSI, absent in
/// some distribution builds. Each one that is missing costs exactly one
/// feature (inbound port mapping, an extra vsock bridge, guest rlimits, a
/// console log file) and never the backend.
pub const OPTIONAL: &[&str] = &[
    "krun_free_ctx",
    "krun_set_port_map",
    "krun_add_vsock_port",
    "krun_set_rlimits",
    "krun_set_console_output",
];

fn path_candidates() -> Vec<String> {
    match std::env::var("TENON_LIBKRUN") {
        Ok(path) if !path.trim().is_empty() => vec![path],
        _ => NAMES.iter().map(|name| name.to_string()).collect(),
    }
}

pub fn tried() -> String {
    path_candidates().join(", ")
}

/// Opens libkrun and resolves every entry point. Returns the same precise
/// reason string the sandbox detection reports: which names were tried, or
/// which symbol a library that did open is missing.
pub fn load() -> Result<Api, String> {
    let mut last = String::new();
    for name in path_candidates() {
        match unsafe { Library::new(&name) } {
            Ok(library) => return resolve(library, &name),
            Err(error) => last = format!("{name}: {error}"),
        }
    }
    Err(format!("libkrun not found (tried {}): {last}", tried()))
}

fn resolve(library: Library, name: &str) -> Result<Api, String> {
    unsafe fn need<T: Copy>(library: &Library, symbol: &str) -> Result<T, String> {
        let bytes = format!("{symbol}\0");
        let found: Symbol<T> = unsafe { library.get(bytes.as_bytes()) }
            .map_err(|error| format!("{symbol} missing: {error}"))?;
        Ok(*found)
    }
    unsafe fn maybe<T: Copy>(library: &Library, symbol: &str) -> Option<T> {
        let bytes = format!("{symbol}\0");
        let found: Symbol<T> = unsafe { library.get(bytes.as_bytes()) }.ok()?;
        Some(*found)
    }
    unsafe {
        let api = Api {
            set_log_level: need(&library, "krun_set_log_level")
                .map_err(|error| format!("{name}: {error}"))?,
            create_ctx: need(&library, "krun_create_ctx")
                .map_err(|error| format!("{name}: {error}"))?,
            free_ctx: maybe(&library, "krun_free_ctx"),
            set_vm_config: need(&library, "krun_set_vm_config")
                .map_err(|error| format!("{name}: {error}"))?,
            set_root: need(&library, "krun_set_root")
                .map_err(|error| format!("{name}: {error}"))?,
            set_workdir: need(&library, "krun_set_workdir")
                .map_err(|error| format!("{name}: {error}"))?,
            set_exec: need(&library, "krun_set_exec")
                .map_err(|error| format!("{name}: {error}"))?,
            set_env: need(&library, "krun_set_env").map_err(|error| format!("{name}: {error}"))?,
            add_virtiofs: need(&library, "krun_add_virtiofs")
                .map_err(|error| format!("{name}: {error}"))?,
            set_port_map: maybe(&library, "krun_set_port_map"),
            add_vsock_port: maybe(&library, "krun_add_vsock_port"),
            set_rlimits: maybe(&library, "krun_set_rlimits"),
            set_console_output: maybe(&library, "krun_set_console_output"),
            start_enter: need(&library, "krun_start_enter")
                .map_err(|error| format!("{name}: {error}"))?,
            _library: library,
        };
        Ok(api)
    }
}
