//! Typed, bounded Linux lifecycle device operations.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use rustix::{
    fd::AsFd,
    io,
    ioctl::{self, NoArg, Updater},
};

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("lifecycle UAPI is validated only for Linux x64 and aa64");

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WatchdogInfo {
    pub options: u32,
    pub firmware_version: u32,
    pub identity: [u8; 32],
}

#[repr(C)]
struct LoopInfo {
    device: u64,
    inode: u64,
    rdevice: u64,
    offset: u64,
    size_limit: u64,
    number: u32,
    encryption: u32,
    key_size: u32,
    flags: u32,
    file_name: [u8; 64],
    crypt_name: [u8; 64],
    key: [u8; 32],
    init: [u64; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopStatus {
    pub backing_device: u64,
    pub inode: u64,
    pub offset: u64,
    pub size_limit: u64,
    pub number: u32,
    pub flags: u32,
}

const GET_SUPPORT: u32 = 0x80285700;
const GET_TIMEOUT: u32 = 0x80045707;
const KEEPALIVE: u32 = 0x80045705;
const GET_LOOP: u32 = 0x4c05;
const CLEAR_LOOP: u32 = 0x4c01;

const _: () = {
    assert!(size_of::<WatchdogInfo>() == 40 && align_of::<WatchdogInfo>() == 4);
    assert!(size_of::<LoopInfo>() == 232 && align_of::<LoopInfo>() == 8);
    assert!(std::mem::offset_of!(LoopInfo, flags) == 52);
    assert!(std::mem::offset_of!(LoopInfo, file_name) == 56);
    assert!(std::mem::offset_of!(LoopInfo, init) == 216);
    assert!(GET_SUPPORT == ioctl::opcode::read::<WatchdogInfo>(b'W', 0));
    assert!(GET_TIMEOUT == ioctl::opcode::read::<i32>(b'W', 7));
    assert!(KEEPALIVE == ioctl::opcode::read::<i32>(b'W', 5));
};

/// Read Linux watchdog capabilities; errors do not imply that it is armed.
#[allow(unsafe_code)]
pub fn watchdog_support(fd: impl AsFd) -> io::Result<WatchdogInfo> {
    let mut value = WatchdogInfo::default();
    // SAFETY: Linux WDIOC_GETSUPPORT writes exactly the initialized repr(C)
    // 40-byte watchdog_info. Updater exclusively borrows its buffer for the
    // synchronous call; AsFd keeps the descriptor alive. No pointers escape.
    unsafe {
        ioctl::ioctl(fd, Updater::<GET_SUPPORT, _>::new(&mut value))?;
    }
    Ok(value)
}

/// Read the actual timeout; never substitute an environment value.
#[allow(unsafe_code)]
pub fn watchdog_timeout(fd: impl AsFd) -> io::Result<u64> {
    let mut value = 0_i32;
    // SAFETY: Linux WDIOC_GETTIMEOUT writes one initialized C int (i32 on
    // both supported ABIs). The unique borrow and live fd last through ioctl.
    unsafe {
        ioctl::ioctl(fd, Updater::<GET_TIMEOUT, _>::new(&mut value))?;
    }
    u64::try_from(value).map_err(|_| io::Errno::INVAL)
}

/// Kick an already opened watchdog; propagate all driver failures.
#[allow(unsafe_code)]
pub fn watchdog_keepalive(fd: impl AsFd) -> io::Result<()> {
    let mut value = 0_i32;
    // SAFETY: Linux WDIOC_KEEPALIVE takes no payload; an initialized C int
    // also satisfies its historical encoded size. Updater conservatively
    // allows kernel writes; the borrowed fd and buffer outlive the call.
    unsafe { ioctl::ioctl(fd, Updater::<KEEPALIVE, _>::new(&mut value)) }
}

/// Inspect the current association on this descriptor, not a backing pathname.
#[allow(unsafe_code)]
pub fn loop_status(fd: impl AsFd) -> io::Result<Option<LoopStatus>> {
    let mut value = LoopInfo {
        device: 0,
        inode: 0,
        rdevice: 0,
        offset: 0,
        size_limit: 0,
        number: 0,
        encryption: 0,
        key_size: 0,
        flags: 0,
        file_name: [0; 64],
        crypt_name: [0; 64],
        key: [0; 32],
        init: [0; 2],
    };
    // SAFETY: LOOP_GET_STATUS64 copies the Linux 232-byte repr(C) loop_info64.
    // Every field is initialized and integer/byte arrays accept all bit
    // patterns. Updater owns the only mutable borrow; fd remains alive until
    // kernel writes finish. The fixed request has no user-supplied pointers.
    match unsafe { ioctl::ioctl(fd, Updater::<GET_LOOP, _>::new(&mut value)) } {
        Ok(()) => Ok(Some(LoopStatus {
            backing_device: value.device,
            inode: value.inode,
            offset: value.offset,
            size_limit: value.size_limit,
            number: value.number,
            flags: value.flags,
        })),
        Err(io::Errno::NXIO) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Request detach on the same validated descriptor. Success can be deferred
/// autoclear; the caller must close it and freshly prove association absence.
#[allow(unsafe_code)]
pub fn clear_loop(fd: impl AsFd) -> io::Result<()> {
    // SAFETY: Linux LOOP_CLR_FD has no pointer argument or kernel memory
    // writes into userspace. NoArg passes null; AsFd keeps the validated
    // descriptor alive for this synchronous, fixed operation.
    unsafe { ioctl::ioctl(fd, NoArg::<CLEAR_LOOP>::new()) }
}
