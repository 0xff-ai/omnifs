#![allow(unsafe_code)]

use std::ffi::{CString, c_char, c_int};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use libloading::Library;

use crate::Error;

const DISK_FORMAT_RAW: u32 = 0;
const SYNC_RELAXED: u32 = 1;

type InitLog = unsafe extern "C" fn(c_int, u32, u32, u32) -> i32;
type CreateContext = unsafe extern "C" fn() -> i32;
type FreeContext = unsafe extern "C" fn(u32) -> i32;
type HasFeature = unsafe extern "C" fn(u64) -> i32;
type SetFirmware = unsafe extern "C" fn(u32, *const c_char) -> i32;
type SetVmConfig = unsafe extern "C" fn(u32, u8, u32) -> i32;
type AddDisk = unsafe extern "C" fn(u32, *const c_char, *const c_char, u32, bool, bool, u32) -> i32;
type DisableImplicitVsock = unsafe extern "C" fn(u32) -> i32;
type AddVsock = unsafe extern "C" fn(u32, u32) -> i32;
type AddVsockPort = unsafe extern "C" fn(u32, u32, *const c_char, bool) -> i32;
type SetConsoleOutput = unsafe extern "C" fn(u32, *const c_char) -> i32;
type GetShutdownFd = unsafe extern "C" fn(u32) -> i32;
type StartEnter = unsafe extern "C" fn(u32) -> i32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feature {
    Block,
    Efi,
    Gpu,
}

impl Feature {
    const fn id(self) -> u64 {
        match self {
            Self::Block => 1,
            Self::Gpu => 2,
            Self::Efi => 5,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Efi => "EFI",
            Self::Gpu => "GPU",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disk {
    Root,
    Seed,
}

impl Disk {
    const fn id(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Seed => "seed",
        }
    }

    const fn read_only(self) -> bool {
        matches!(self, Self::Seed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortDirection {
    GuestConnects,
    HostConnects,
}

impl PortDirection {
    const fn libkrun_listen(self) -> bool {
        matches!(self, Self::HostConnects)
    }
}

pub(crate) trait Api {
    fn init_log(&self, target: RawFd) -> Result<(), Error>;
    fn has_feature(&self, feature: Feature) -> Result<bool, Error>;
    fn create_context(&self) -> Result<u32, Error>;
    fn free_context(&self, context: u32);
    fn set_firmware(&self, context: u32, path: &Path) -> Result<(), Error>;
    fn set_vm_config(&self, context: u32, vcpus: u8, memory_mib: u32) -> Result<(), Error>;
    fn add_disk(&self, context: u32, disk: Disk, path: &Path) -> Result<(), Error>;
    fn disable_implicit_vsock(&self, context: u32) -> Result<(), Error>;
    fn add_vsock(&self, context: u32) -> Result<(), Error>;
    fn add_vsock_port(
        &self,
        context: u32,
        port: u32,
        path: &Path,
        direction: PortDirection,
    ) -> Result<(), Error>;
    fn set_console_output(&self, context: u32, path: &Path) -> Result<(), Error>;
    fn shutdown_fd(&self, context: u32) -> Result<ShutdownFd, Error>;
    fn start_enter(&self, context: u32) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShutdownFd(RawFd);

impl ShutdownFd {
    pub(crate) const fn from_raw(fd: RawFd) -> Self {
        Self(fd)
    }

    pub(crate) fn signal(self) -> Result<(), Error> {
        let value = 1_u64.to_ne_bytes();
        // SAFETY: libkrun returned this live write descriptor for the
        // configured context, and the fixed eight-byte event value matches
        // its eventfd API.
        let written = unsafe { libc::write(self.0, value.as_ptr().cast(), value.len()) };
        if written == value.len().cast_signed() {
            Ok(())
        } else if written < 0 {
            Err(Error::Control(format!(
                "write shutdown eventfd: {}",
                std::io::Error::last_os_error()
            )))
        } else {
            Err(Error::Control(format!(
                "short shutdown eventfd write: wrote {written} of {} bytes",
                value.len()
            )))
        }
    }
}

struct Call {
    function: &'static str,
    code: i32,
}

impl Call {
    fn check(self) -> Result<(), Error> {
        match self.code.cmp(&0) {
            std::cmp::Ordering::Less => Err(Error::Call {
                function: self.function,
                code: self.code,
            }),
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Greater => Err(Error::UnexpectedReturn {
                function: self.function,
                value: self.code,
            }),
        }
    }
}

pub(crate) struct LibraryApi {
    path: PathBuf,
    _library: Library,
    init_log: InitLog,
    create_context: CreateContext,
    free_context: FreeContext,
    has_feature: HasFeature,
    set_firmware: SetFirmware,
    set_vm_config: SetVmConfig,
    add_disk: AddDisk,
    disable_implicit_vsock: DisableImplicitVsock,
    add_vsock: AddVsock,
    add_vsock_port: AddVsockPort,
    set_console_output: SetConsoleOutput,
    get_shutdown_fd: GetShutdownFd,
    start_enter: StartEnter,
}

impl LibraryApi {
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute() {
            return Err(Error::Config(format!(
                "libkrun dylib path must be absolute: {}",
                path.display()
            )));
        }
        // SAFETY: the helper accepts only the exact absolute dylib path from
        // its validated closed configuration. Every loaded symbol is copied
        // into the table below and the owning library stays alive with it.
        let library = unsafe { Library::new(path) }.map_err(|source| Error::LoadLibrary {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: signatures match libkrun 1.19.4's public C header. Loading
        // every required symbol up front makes ABI drift fail before a VM is
        // configured or a pidfile is published.
        unsafe {
            Ok(Self {
                path: path.to_path_buf(),
                init_log: Self::symbol(&library, b"krun_init_log\0", "krun_init_log", path)?,
                create_context: Self::symbol(
                    &library,
                    b"krun_create_ctx\0",
                    "krun_create_ctx",
                    path,
                )?,
                free_context: Self::symbol(&library, b"krun_free_ctx\0", "krun_free_ctx", path)?,
                has_feature: Self::symbol(
                    &library,
                    b"krun_has_feature\0",
                    "krun_has_feature",
                    path,
                )?,
                set_firmware: Self::symbol(
                    &library,
                    b"krun_set_firmware\0",
                    "krun_set_firmware",
                    path,
                )?,
                set_vm_config: Self::symbol(
                    &library,
                    b"krun_set_vm_config\0",
                    "krun_set_vm_config",
                    path,
                )?,
                add_disk: Self::symbol(&library, b"krun_add_disk3\0", "krun_add_disk3", path)?,
                disable_implicit_vsock: Self::symbol(
                    &library,
                    b"krun_disable_implicit_vsock\0",
                    "krun_disable_implicit_vsock",
                    path,
                )?,
                add_vsock: Self::symbol(&library, b"krun_add_vsock\0", "krun_add_vsock", path)?,
                add_vsock_port: Self::symbol(
                    &library,
                    b"krun_add_vsock_port2\0",
                    "krun_add_vsock_port2",
                    path,
                )?,
                set_console_output: Self::symbol(
                    &library,
                    b"krun_set_console_output\0",
                    "krun_set_console_output",
                    path,
                )?,
                get_shutdown_fd: Self::symbol(
                    &library,
                    b"krun_get_shutdown_eventfd\0",
                    "krun_get_shutdown_eventfd",
                    path,
                )?,
                start_enter: Self::symbol(
                    &library,
                    b"krun_start_enter\0",
                    "krun_start_enter",
                    path,
                )?,
                _library: library,
            })
        }
    }

    unsafe fn symbol<T: Copy>(
        library: &Library,
        raw_name: &'static [u8],
        name: &'static str,
        path: &Path,
    ) -> Result<T, Error> {
        // SAFETY: the caller supplies a nul-terminated symbol name and the
        // exact function-pointer type declared by libkrun's C header.
        let symbol =
            unsafe { library.get::<T>(raw_name) }.map_err(|source| Error::MissingSymbol {
                path: path.to_path_buf(),
                symbol: name,
                source,
            })?;
        Ok(*symbol)
    }

    fn c_path(path: &Path) -> Result<CString, Error> {
        use std::os::unix::ffi::OsStrExt as _;

        CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::PathContainsNul(path.into()))
    }
}

impl Api for LibraryApi {
    fn init_log(&self, target: RawFd) -> Result<(), Error> {
        // Warn level, no terminal styling, and no environment override.
        // SAFETY: `target` remains open for the life of the VM.
        Call {
            function: "krun_init_log",
            code: unsafe { (self.init_log)(target, 2, 2, 1) },
        }
        .check()
    }

    fn has_feature(&self, feature: Feature) -> Result<bool, Error> {
        // SAFETY: feature IDs come from libkrun 1.19.4's public header.
        let value = unsafe { (self.has_feature)(feature.id()) };
        match value {
            value if value < 0 => Err(Error::Call {
                function: "krun_has_feature",
                code: value,
            }),
            0 => Ok(false),
            1 => Ok(true),
            value => Err(Error::UnexpectedReturn {
                function: "krun_has_feature",
                value,
            }),
        }
    }

    fn create_context(&self) -> Result<u32, Error> {
        // SAFETY: the function takes no arguments.
        let context = unsafe { (self.create_context)() };
        u32::try_from(context).map_err(|_| Error::Call {
            function: "krun_create_ctx",
            code: context,
        })
    }

    fn free_context(&self, context: u32) {
        // SAFETY: `context` was returned by this library instance.
        let _ = unsafe { (self.free_context)(context) };
    }

    fn set_firmware(&self, context: u32, path: &Path) -> Result<(), Error> {
        let path = Self::c_path(path)?;
        // SAFETY: the C string lives through the call.
        Call {
            function: "krun_set_firmware",
            code: unsafe { (self.set_firmware)(context, path.as_ptr()) },
        }
        .check()
    }

    fn set_vm_config(&self, context: u32, vcpus: u8, memory_mib: u32) -> Result<(), Error> {
        // SAFETY: all values are plain C integer arguments.
        Call {
            function: "krun_set_vm_config",
            code: unsafe { (self.set_vm_config)(context, vcpus, memory_mib) },
        }
        .check()
    }

    fn add_disk(&self, context: u32, disk: Disk, path: &Path) -> Result<(), Error> {
        let id = CString::new(disk.id()).expect("fixed disk IDs contain no NUL");
        let path = Self::c_path(path)?;
        // SAFETY: both C strings live through the call. The disk format and
        // sync mode constants come from libkrun 1.19.4's public header.
        Call {
            function: "krun_add_disk3",
            code: unsafe {
                (self.add_disk)(
                    context,
                    id.as_ptr(),
                    path.as_ptr(),
                    DISK_FORMAT_RAW,
                    disk.read_only(),
                    false,
                    SYNC_RELAXED,
                )
            },
        }
        .check()
    }

    fn disable_implicit_vsock(&self, context: u32) -> Result<(), Error> {
        // SAFETY: `context` is live.
        Call {
            function: "krun_disable_implicit_vsock",
            code: unsafe { (self.disable_implicit_vsock)(context) },
        }
        .check()
    }

    fn add_vsock(&self, context: u32) -> Result<(), Error> {
        // TSI feature mask 0 forbids both inet and Unix hijacking.
        // SAFETY: `context` is live.
        Call {
            function: "krun_add_vsock",
            code: unsafe { (self.add_vsock)(context, 0) },
        }
        .check()
    }

    fn add_vsock_port(
        &self,
        context: u32,
        port: u32,
        path: &Path,
        direction: PortDirection,
    ) -> Result<(), Error> {
        let path = Self::c_path(path)?;
        // SAFETY: the C string lives through the call.
        Call {
            function: "krun_add_vsock_port2",
            code: unsafe {
                (self.add_vsock_port)(context, port, path.as_ptr(), direction.libkrun_listen())
            },
        }
        .check()
    }

    fn set_console_output(&self, context: u32, path: &Path) -> Result<(), Error> {
        let path = Self::c_path(path)?;
        // SAFETY: the C string lives through the call.
        Call {
            function: "krun_set_console_output",
            code: unsafe { (self.set_console_output)(context, path.as_ptr()) },
        }
        .check()
    }

    fn shutdown_fd(&self, context: u32) -> Result<ShutdownFd, Error> {
        // SAFETY: `context` is live.
        let fd = unsafe { (self.get_shutdown_fd)(context) };
        if fd < 0 {
            Err(Error::Call {
                function: "krun_get_shutdown_eventfd",
                code: fd,
            })
        } else {
            Ok(ShutdownFd::from_raw(fd))
        }
    }

    fn start_enter(&self, context: u32) -> i32 {
        // SAFETY: all configuration is complete and the context is consumed
        // by this call. Normal VM operation never returns.
        unsafe { (self.start_enter)(context) }
    }
}

impl std::fmt::Debug for LibraryApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibraryApi")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}
