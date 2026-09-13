//! Typed, bounded Linux lifecycle device operations.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use rustix::{
    fd::AsFd,
    io,
    ioctl::{self, NoArg, Updater},
};

mod startup;
pub use startup::{
    DmCreated, dm_create, dm_discard_created, dm_load_verity, dm_resume, loop_attach, loop_free,
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

// Linux DM v4 uses a 312-byte request header but returns only 305 header bytes
// for DEV_STATUS/REMOVE. TABLE_STATUS appends 8-byte-aligned target records.
const DM_STATUS_REQUEST: u32 = 0xc138_fd07;
const DM_TABLE_REQUEST: u32 = 0xc138_fd0c;
const DM_REMOVE_REQUEST: u32 = 0xc138_fd04;
const DM_READONLY: u32 = 1;
const DM_TABLE: u32 = 1 << 4;
const DM_ACTIVE: u32 = 1 << 5;
const DM_FULL: u32 = 1 << 8;
const DM_UEVENT: u32 = 1 << 13;
const DM_CAPACITY: usize = 65536;

#[repr(C)]
struct DmHeader {
    version: [u32; 3],
    data_size: u32,
    data_start: u32,
    target_count: u32,
    open_count: i32,
    flags: u32,
    event_nr: u32,
    padding: u32,
    dev: u64,
    name: [u8; 128],
    uuid: [u8; 129],
    data: [u8; 7],
}
#[repr(C)]
struct DmBuffer {
    header: DmHeader,
    data: [u8; DM_CAPACITY - 312],
}
const _: () = {
    assert!(size_of::<DmHeader>() == 312 && align_of::<DmHeader>() == 8);
    assert!(std::mem::offset_of!(DmHeader, dev) == 40);
    assert!(std::mem::offset_of!(DmHeader, name) == 48);
    assert!(std::mem::offset_of!(DmHeader, uuid) == 176);
    assert!(std::mem::offset_of!(DmHeader, data) == 305);
    assert!(size_of::<DmBuffer>() == DM_CAPACITY && align_of::<DmBuffer>() == 8);
    assert!(std::mem::offset_of!(DmBuffer, data) == 312);
    assert!(DM_STATUS_REQUEST == ioctl::opcode::read_write::<DmHeader>(0xfd, 7));
    assert!(DM_TABLE_REQUEST == ioctl::opcode::read_write::<DmHeader>(0xfd, 12));
    assert!(DM_REMOVE_REQUEST == ioctl::opcode::read_write::<DmHeader>(0xfd, 4));
};

/// Checked active, read-only DM identity. Event/count comparisons detect table
/// changes between status and table/removal requests; this is not a release token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmStatus {
    pub device: u64,
    pub name: String,
    pub uuid: String,
    pub targets: u32,
    pub open_count: i32,
    pub event: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmTarget {
    pub sector: u64,
    pub length: u64,
    pub kind: String,
    pub parameters: String,
}

fn put_string(target: &mut [u8], value: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() >= target.len()
        || !value.bytes().all(|c| c.is_ascii_graphic())
    {
        return Err(io::Errno::INVAL);
    }
    target.fill(0);
    target[..value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}
fn dm_string(bytes: &[u8]) -> io::Result<&str> {
    let end = bytes.iter().position(|b| *b == 0).ok_or(io::Errno::PROTO)?;
    if end == 0 || !bytes[..end].iter().all(u8::is_ascii_graphic) {
        return Err(io::Errno::PROTO);
    }
    std::str::from_utf8(&bytes[..end]).map_err(|_| io::Errno::PROTO)
}
impl DmBuffer {
    fn request(size: usize, device: u64, uuid: Option<&str>, table: bool) -> io::Result<Self> {
        if !(312..=DM_CAPACITY).contains(&size) || (device == 0) == uuid.is_none() {
            return Err(io::Errno::INVAL);
        }
        let mut buffer = Self {
            header: DmHeader {
                // These three operations are defined by DM ABI v4.0, used by
                // all current board kernels (including the selected 5.15 BSP).
                // No newer feature or inactive/deferred operation is requested.
                version: [4, 0, 0],
                data_size: size as u32,
                data_start: 312,
                target_count: 0,
                open_count: 0,
                flags: if table { DM_TABLE } else { 0 },
                event_nr: 0,
                padding: 0,
                dev: device,
                name: [0; 128],
                uuid: [0; 129],
                data: [0; 7],
            },
            data: [0; DM_CAPACITY - 312],
        };
        if let Some(uuid) = uuid {
            put_string(&mut buffer.header.uuid, uuid)?;
        }
        Ok(buffer)
    }
}
fn valid_header(buffer: &DmBuffer, capacity: usize) -> io::Result<()> {
    let h = &buffer.header;
    if h.version[0] != 4
        || !(305..=capacity).contains(&(h.data_size as usize))
        || h.data_start != 312
        || h.padding != 0
    {
        return Err(io::Errno::PROTO);
    }
    Ok(())
}
fn parse_dm_status(buffer: &DmBuffer, capacity: usize, table: bool) -> io::Result<DmStatus> {
    valid_header(buffer, capacity)?;
    let h = &buffer.header;
    let required = DM_READONLY | DM_ACTIVE | if table { DM_TABLE } else { 0 };
    let allowed = required | if table { DM_FULL } else { 0 };
    if h.flags & required != required
        || h.flags & !allowed != 0
        || !(1..=16).contains(&h.target_count)
        || h.open_count < 0
        || (!table && h.data_size != 305)
        || h.dev == 0
    {
        return Err(io::Errno::PROTO);
    }
    Ok(DmStatus {
        device: h.dev,
        name: dm_string(&h.name)?.into(),
        uuid: dm_string(&h.uuid)?.into(),
        targets: h.target_count,
        open_count: h.open_count,
        event: h.event_nr,
    })
}
fn parse_dm_table(
    buffer: &DmBuffer,
    capacity: usize,
    expected: &DmStatus,
) -> io::Result<Vec<DmTarget>> {
    if &parse_dm_status(buffer, capacity, true)? != expected || buffer.header.flags & DM_FULL != 0 {
        return Err(io::Errno::PROTO);
    }
    let end = (buffer.header.data_size as usize)
        .checked_sub(312)
        .ok_or(io::Errno::PROTO)?;
    let data = buffer.data.get(..end).ok_or(io::Errno::PROTO)?;
    let mut at: usize = 0;
    let mut sector_end = 0_u64;
    let mut targets = Vec::new();
    for number in 0..expected.targets {
        let raw = data
            .get(at..at.checked_add(40).ok_or(io::Errno::PROTO)?)
            .ok_or(io::Errno::PROTO)?;
        let sector = u64::from_ne_bytes(raw[0..8].try_into().map_err(|_| io::Errno::PROTO)?);
        let length = u64::from_ne_bytes(raw[8..16].try_into().map_err(|_| io::Errno::PROTO)?);
        let status = i32::from_ne_bytes(raw[16..20].try_into().map_err(|_| io::Errno::PROTO)?);
        let next =
            u32::from_ne_bytes(raw[20..24].try_into().map_err(|_| io::Errno::PROTO)?) as usize;
        let last = number + 1 == expected.targets;
        // TABLE_STATUS next is relative to the FIRST spec, even for later
        // entries. Its final value includes alignment beyond data_size.
        if sector != sector_end
            || length == 0
            || status != 0
            || next <= at + 40
            || !next.is_multiple_of(8)
            || next > capacity - 312
            || (!last && next >= end)
            || (last && next != end.next_multiple_of(8))
        {
            return Err(io::Errno::PROTO);
        }
        sector_end = sector.checked_add(length).ok_or(io::Errno::PROTO)?;
        let params = data
            .get(at + 40..if last { end } else { next })
            .ok_or(io::Errno::PROTO)?;
        let nul = params
            .iter()
            .position(|b| *b == 0)
            .ok_or(io::Errno::PROTO)?;
        if params[..nul]
            .iter()
            .any(|c| !c.is_ascii_graphic() && *c != b' ')
            || (last && at + 40 + nul + 1 != end)
            || (at + 40 + nul + 1).next_multiple_of(8) != next
        {
            return Err(io::Errno::PROTO);
        }
        targets.push(DmTarget {
            sector,
            length,
            kind: dm_string(&raw[24..40])?.into(),
            parameters: std::str::from_utf8(&params[..nul])
                .map_err(|_| io::Errno::PROTO)?
                .into(),
        });
        at = next;
    }
    Ok(targets)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmCommand {
    Status,
    Table,
    Remove,
    Create,
    Load,
    Resume,
}

#[allow(unsafe_code)]
fn dm_exchange(fd: impl AsFd, command: DmCommand, buffer: &mut DmBuffer) -> io::Result<()> {
    // SAFETY: These fixed Linux DM requests encode the repr(C) 312-byte
    // header, followed by at most data_size initialized bytes. The full 65536
    // byte repr(C), 8-aligned object is live and exclusively borrowed throughout
    // ioctl; data_size is constructed internally and never exceeds that object.
    // Every kernel-writable field is an integer/byte array accepting all bit
    // patterns. Updater exposes no pointer outside this call; AsFd retains the
    // verified control descriptor. No user-selected request number is accepted.
    unsafe {
        match command {
            DmCommand::Status => ioctl::ioctl(fd, Updater::<DM_STATUS_REQUEST, _>::new(buffer)),
            DmCommand::Table => ioctl::ioctl(fd, Updater::<DM_TABLE_REQUEST, _>::new(buffer)),
            DmCommand::Remove => ioctl::ioctl(fd, Updater::<DM_REMOVE_REQUEST, _>::new(buffer)),
            DmCommand::Create => ioctl::ioctl(
                fd,
                Updater::<{ startup::DM_CREATE_REQUEST }, _>::new(buffer),
            ),
            DmCommand::Load => {
                ioctl::ioctl(fd, Updater::<{ startup::DM_LOAD_REQUEST }, _>::new(buffer))
            }
            DmCommand::Resume => ioctl::ioctl(
                fd,
                Updater::<{ startup::DM_RESUME_REQUEST }, _>::new(buffer),
            ),
        }
    }
}
fn read_dm_status(
    device: u64,
    exchange: &mut impl FnMut(DmCommand, &mut DmBuffer) -> io::Result<()>,
) -> io::Result<DmStatus> {
    for _ in 0..4 {
        let mut buffer = DmBuffer::request(312, device, None, false)?;
        match exchange(DmCommand::Status, &mut buffer) {
            Err(io::Errno::INTR) => continue,
            result => result?,
        }
        let status = parse_dm_status(&buffer, 312, false)?;
        if status.device != device {
            return Err(io::Errno::PROTO);
        }
        return Ok(status);
    }
    Err(io::Errno::INTR)
}
fn read_dm_table(
    expected: &DmStatus,
    exchange: &mut impl FnMut(DmCommand, &mut DmBuffer) -> io::Result<()>,
) -> io::Result<Vec<DmTarget>> {
    let mut capacity = 1024;
    let mut failure = io::Errno::OVERFLOW;
    for _ in 0..4 {
        let mut buffer = DmBuffer::request(capacity, 0, Some(&expected.uuid), true)?;
        match exchange(DmCommand::Table, &mut buffer) {
            Err(io::Errno::INTR) => {
                failure = io::Errno::INTR;
                continue;
            }
            result => result?,
        }
        failure = io::Errno::OVERFLOW;
        if &parse_dm_status(&buffer, capacity, true)? != expected {
            return Err(io::Errno::PROTO);
        }
        if buffer.header.flags & DM_FULL == 0 {
            return parse_dm_table(&buffer, capacity, expected);
        }
        capacity = (capacity * 4).min(DM_CAPACITY);
    }
    Err(failure)
}
fn remove_dm(
    expected: &DmStatus,
    exchange: &mut impl FnMut(DmCommand, &mut DmBuffer) -> io::Result<()>,
) -> io::Result<()> {
    let current = read_dm_status(expected.device, exchange)?;
    if current.open_count != 0 {
        return Err(io::Errno::BUSY);
    }
    if &current != expected {
        return Err(io::Errno::PROTO);
    }
    let mut buffer = DmBuffer::request(312, 0, Some(&expected.uuid), false)?;
    // Never blindly retry a mutating ioctl, including EINTR: the supervisor
    // reobserves the complete live graph even when an operation returned error.
    exchange(DmCommand::Remove, &mut buffer)?;
    valid_header(&buffer, 312)?;
    let h = &buffer.header;
    // dev_remove fills name/uuid but does not call __dev_status. dev remains
    // zero for a UUID-selected request; it is not a post-removal device proof.
    if h.data_size != 305
        || h.dev != 0
        || h.target_count != 0
        || h.open_count != 0
        || h.flags & !DM_UEVENT != 0
        || dm_string(&h.name)? != expected.name
        || dm_string(&h.uuid)? != expected.uuid
    {
        return Err(io::Errno::PROTO);
    }
    Ok(())
}

/// Inspect active read-only DM state using its current encoded device identity.
pub fn dm_status(fd: impl AsFd, device: u64) -> io::Result<DmStatus> {
    read_dm_status(device, &mut |command, buffer| {
        dm_exchange(fd.as_fd(), command, buffer)
    })
}
/// Read the complete table, refusing identity/event/count races or partial output.
pub fn dm_table(fd: impl AsFd, expected: &DmStatus) -> io::Result<Vec<DmTarget>> {
    read_dm_table(expected, &mut |command, buffer| {
        dm_exchange(fd.as_fd(), command, buffer)
    })
}
/// Remove exactly the revalidated UUID on the same control FD, without force,
/// defer, cookies or blind retries. Caller must separately prove disappearance.
pub fn dm_remove(fd: impl AsFd, expected: &DmStatus) -> io::Result<()> {
    remove_dm(expected, &mut |command, buffer| {
        dm_exchange(fd.as_fd(), command, buffer)
    })
}

#[cfg(test)]
mod dm_tests {
    use super::*;

    fn status() -> DmStatus {
        DmStatus {
            device: 0xfd00,
            name: "mica-root".into(),
            uuid: "CRYPT-VERITY-owned".into(),
            targets: 2,
            open_count: 0,
            event: 7,
        }
    }
    fn reply(table: bool) -> DmBuffer {
        let mut b = DmBuffer::request(4096, 0xfd00, None, table).unwrap();
        b.header.version = [4, 48, 0];
        b.header.data_size = 305;
        b.header.flags = DM_READONLY | DM_ACTIVE | if table { DM_TABLE } else { 0 };
        b.header.target_count = 2;
        b.header.event_nr = 7;
        put_string(&mut b.header.name, "mica-root").unwrap();
        put_string(&mut b.header.uuid, "CRYPT-VERITY-owned").unwrap();
        b
    }
    fn targets(b: &mut DmBuffer) {
        let mut at: usize = 0;
        for (sector, parameters) in [(0_u64, "1 7:0 7:0"), (8, "1 7:1 7:1 extended")] {
            let used = at + 40 + parameters.len() + 1;
            let next = used.next_multiple_of(8);
            b.data[at..at + 8].copy_from_slice(&sector.to_ne_bytes());
            b.data[at + 8..at + 16].copy_from_slice(&8_u64.to_ne_bytes());
            b.data[at + 20..at + 24].copy_from_slice(&(next as u32).to_ne_bytes());
            b.data[at + 24..at + 31].copy_from_slice(b"verity\0");
            b.data[at + 40..used - 1].copy_from_slice(parameters.as_bytes());
            b.header.data_size = (312 + used) as u32;
            at = next;
        }
    }
    #[test]
    fn dm_status_and_table_use_kernel_lengths_and_absolute_next_offsets() {
        let mut b = reply(false);
        assert_eq!(parse_dm_status(&b, 4096, false).unwrap(), status());
        b = reply(true);
        targets(&mut b);
        let t = parse_dm_table(&b, 4096, &status()).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[1].sector, 8);
        assert_eq!(t[1].length, 8);
        assert_eq!(t[1].parameters, "1 7:1 7:1 extended");
        assert!(!b.header.data_size.is_multiple_of(8));
    }
    #[test]
    fn dm_readonly_flag_is_required_in_status_and_table() {
        for table in [false, true] {
            let mut b = reply(table);
            assert!(parse_dm_status(&b, 4096, table).is_ok());
            b.header.flags &= !DM_READONLY;
            assert_eq!(
                parse_dm_status(&b, 4096, table).unwrap_err(),
                io::Errno::PROTO
            );
        }
    }

    #[test]
    fn dm_status_rejects_non_kernel_reply_lengths() {
        for size in [304, 306, 312, 313] {
            let mut b = reply(false);
            b.header.data_size = size;
            assert!(parse_dm_status(&b, 312, false).is_err(), "size={size}");
        }
    }

    #[test]
    fn dm_refuses_malformed_header_flags_counts_and_identity() {
        for change in 0..13 {
            let mut b = reply(true);
            targets(&mut b);
            match change {
                0 => b.header.version[0] = 5,
                1 => b.header.data_size = 304,
                2 => b.header.data_size = 65537,
                3 => b.header.data_start = 311,
                4 => b.header.flags |= 1 << 17,
                5 => b.header.flags |= 1 << 6,
                6 => b.header.flags &= !DM_TABLE,
                7 => b.header.target_count = 0,
                8 => b.header.target_count = 17,
                9 => b.header.name.fill(b'x'),
                10 => b.header.uuid.fill(b'x'),
                11 => b.header.dev = 0xfd01,
                _ => b.header.open_count = -1,
            }
            assert!(
                parse_dm_table(&b, 4096, &status()).is_err(),
                "change={change}"
            );
        }
        for change in 0..4 {
            let mut b = reply(true);
            targets(&mut b);
            match change {
                0 => b.header.name[0] = b'x',
                1 => b.header.uuid[0] = b'x',
                2 => b.header.event_nr += 1,
                _ => b.header.target_count = 1,
            }
            assert!(parse_dm_table(&b, 4096, &status()).is_err());
        }
    }
    #[test]
    fn dm_refuses_truncated_unterminated_overflowing_and_incomplete_targets() {
        for change in 0..12 {
            let mut b = reply(true);
            targets(&mut b);
            let second = u32::from_ne_bytes(b.data[20..24].try_into().unwrap()) as usize;
            let end = b.header.data_size as usize - 312;
            match change {
                0 => b.header.data_size = 312 + 39,
                1 => b.data[24..40].fill(b'x'),
                2 => b.data[40..second].fill(b'x'),
                3 => b.data[20..24].copy_from_slice(&0_u32.to_ne_bytes()),
                4 => b.data[20..24].copy_from_slice(&41_u32.to_ne_bytes()),
                5 => b.data[20..24].copy_from_slice(&u32::MAX.to_ne_bytes()),
                6 => b.data[second + 20..second + 24].copy_from_slice(&64_u32.to_ne_bytes()),
                7 => b.data[second..second + 8].copy_from_slice(&9_u64.to_ne_bytes()),
                8 => b.data[8..16].copy_from_slice(&u64::MAX.to_ne_bytes()),
                9 => b.data[16..20].copy_from_slice(&1_i32.to_ne_bytes()),
                10 => b.data[second + 40..end].fill(b'x'),
                _ => b.data[8..16].fill(0),
            }
            assert!(
                parse_dm_table(&b, 4096, &status()).is_err(),
                "change={change}"
            );
        }
    }
    #[test]
    fn dm_buffer_growth_and_interruptions_are_bounded() {
        let mut calls = Vec::new();
        let result = read_dm_table(&status(), &mut |kind, b| {
            assert_eq!(kind, DmCommand::Table);
            calls.push(b.header.data_size);
            *b = reply(true);
            b.header.flags |= DM_FULL;
            Ok(())
        });
        assert_eq!(result.unwrap_err(), io::Errno::OVERFLOW);
        assert_eq!(calls, [1024, 4096, 16384, 65536]);
        let mut count = 0;
        assert_eq!(
            read_dm_status(0xfd00, &mut |_, _| {
                count += 1;
                Err(io::Errno::INTR)
            })
            .unwrap_err(),
            io::Errno::INTR
        );
        assert_eq!(count, 4);
        count = 0;
        assert_eq!(
            read_dm_table(&status(), &mut |_, _| {
                count += 1;
                Err(io::Errno::INTR)
            })
            .unwrap_err(),
            io::Errno::INTR
        );
        assert_eq!(count, 4);
    }
    #[test]
    fn dm_remove_refuses_busy_identity_races_and_bad_replies() {
        for case in 0..5 {
            let mut calls = 0;
            let result = remove_dm(&status(), &mut |kind, b| {
                calls += 1;
                if kind == DmCommand::Status {
                    *b = reply(false);
                    if case == 0 {
                        b.header.open_count = 1;
                    }
                    if case == 1 {
                        b.header.uuid[0] = b'x';
                    }
                } else {
                    assert_eq!(kind, DmCommand::Remove);
                    assert_eq!(b.header.dev, 0);
                    assert_eq!(b.header.flags, 0);
                    assert!(b.header.name.iter().all(|c| *c == 0));
                    assert_eq!(dm_string(&b.header.uuid).unwrap(), "CRYPT-VERITY-owned");
                    if case == 2 {
                        return Err(io::Errno::BUSY);
                    }
                    *b = reply(false);
                    b.header.data_size = 305;
                    b.header.dev = 0;
                    b.header.flags = 1 << 13;
                    b.header.target_count = 0;
                    b.header.event_nr = 0;
                    if case == 3 {
                        b.header.uuid[0] = b'x';
                    }
                }
                Ok(())
            });
            assert_eq!(result.is_ok(), case == 4);
            assert_eq!(calls, if case < 2 { 1 } else { 2 });
        }
    }
}
