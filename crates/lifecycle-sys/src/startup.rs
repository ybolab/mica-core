//! Creation operations kept separate from the stricter retained teardown API.
use super::*;
use rustix::{fd::AsRawFd, ioctl::IntegerSetter};

pub(super) const DM_CREATE_REQUEST: u32 = 0xc138_fd03;
pub(super) const DM_LOAD_REQUEST: u32 = 0xc138_fd09;
pub(super) const DM_RESUME_REQUEST: u32 = 0xc138_fd06;
const DM_INACTIVE: u32 = 1 << 6;
const _: () = {
    assert!(DM_CREATE_REQUEST == ioctl::opcode::read_write::<DmHeader>(0xfd, 3));
    assert!(DM_LOAD_REQUEST == ioctl::opcode::read_write::<DmHeader>(0xfd, 9));
    assert!(DM_RESUME_REQUEST == ioctl::opcode::read_write::<DmHeader>(0xfd, 6));
};

/// A creation receipt, never constructible from a name supplied by a caller.
/// It permits rollback of an inactive mapping that the active-only shutdown
/// interface deliberately refuses. The caller keeps the control FD alive.
#[derive(Debug)]
pub struct DmCreated {
    pub device: u64,
    name: String,
    uuid: String,
}

fn creation_status(b: &DmBuffer, name: &str, uuid: &str) -> io::Result<()> {
    valid_header(b, DM_CAPACITY)?;
    if b.header.data_size != 305
        || b.header.dev == 0
        || b.header.open_count != 0
        || dm_string(&b.header.name)? != name
        || dm_string(&b.header.uuid)? != uuid
    {
        return Err(io::Errno::PROTO);
    }
    Ok(())
}

/// Create only a new named device; existing names/UUIDs are never adopted.
pub fn dm_create(fd: impl AsFd, name: &str, uuid: &str) -> io::Result<DmCreated> {
    let mut b = DmBuffer::request(312, 0, Some(uuid), false)?;
    put_string(&mut b.header.name, name)?;
    dm_exchange(fd, DmCommand::Create, &mut b)?;
    creation_status(&b, name, uuid)?;
    if b.header.flags != 0 || b.header.target_count != 0 {
        return Err(io::Errno::PROTO);
    }
    Ok(DmCreated {
        device: b.header.dev,
        name: name.into(),
        uuid: uuid.into(),
    })
}

fn load_request(created: &DmCreated, target: &DmTarget) -> io::Result<DmBuffer> {
    if target.sector != 0
        || target.length == 0
        || target.kind != "verity"
        || target.parameters.is_empty()
        || target.parameters.len() > 4096
        || target
            .parameters
            .bytes()
            .any(|c| !c.is_ascii_graphic() && c != b' ')
    {
        return Err(io::Errno::INVAL);
    }
    let next = (40 + target.parameters.len() + 1).next_multiple_of(8);
    let mut b = DmBuffer::request(312 + next, 0, Some(&created.uuid), false)?;
    b.header.flags = DM_READONLY;
    b.header.target_count = 1;
    b.data[8..16].copy_from_slice(&target.length.to_ne_bytes());
    // TABLE_LOAD offsets are relative to this spec (only one is permitted).
    b.data[20..24].copy_from_slice(&(next as u32).to_ne_bytes());
    b.data[24..31].copy_from_slice(b"verity\0");
    b.data[40..40 + target.parameters.len()].copy_from_slice(target.parameters.as_bytes());
    Ok(b)
}

/// Load a single read-only verity target. No generic target or writable flag.
pub fn dm_load_verity(fd: impl AsFd, created: &DmCreated, target: &DmTarget) -> io::Result<()> {
    let mut b = load_request(created, target)?;
    dm_exchange(fd, DmCommand::Load, &mut b)?;
    creation_status(&b, &created.name, &created.uuid)?;
    if b.header.dev != created.device || b.header.flags != DM_INACTIVE || b.header.target_count != 0
    {
        return Err(io::Errno::PROTO);
    }
    Ok(())
}

/// Activate the loaded table, then use the existing strict read-only status API.
pub fn dm_resume(fd: impl AsFd, created: &DmCreated) -> io::Result<DmStatus> {
    let mut b = DmBuffer::request(312, 0, Some(&created.uuid), false)?;
    dm_exchange(fd.as_fd(), DmCommand::Resume, &mut b)?;
    creation_status(&b, &created.name, &created.uuid)?;
    if b.header.dev != created.device
        || b.header.flags & !DM_UEVENT != DM_READONLY | DM_ACTIVE
        || b.header.target_count != 1
    {
        return Err(io::Errno::PROTO);
    }
    let status = dm_status(fd, created.device)?;
    if status.name != created.name || status.uuid != created.uuid {
        return Err(io::Errno::PROTO);
    }
    Ok(status)
}

/// Roll back this worker's creation only. Fresh identity/open-count checks
/// precede UUID-selected removal; no force, defer, or mutating retries.
pub fn dm_discard_created(fd: impl AsFd, created: &DmCreated) -> io::Result<()> {
    let mut current = DmBuffer::request(312, created.device, None, false)?;
    dm_exchange(fd.as_fd(), DmCommand::Status, &mut current)?;
    creation_status(&current, &created.name, &created.uuid)?;
    if current.header.dev != created.device
        || current.header.target_count > 1
        || current.header.flags & !(DM_READONLY | DM_ACTIVE | DM_INACTIVE | 2) != 0
    {
        return Err(io::Errno::PROTO);
    }
    let mut b = DmBuffer::request(312, 0, Some(&created.uuid), false)?;
    dm_exchange(fd.as_fd(), DmCommand::Remove, &mut b)?;
    valid_header(&b, 312)?;
    if b.header.data_size != 305
        || b.header.dev != 0
        || b.header.target_count != 0
        || b.header.open_count != 0
        || b.header.flags & !DM_UEVENT != 0
        || dm_string(&b.header.name)? != created.name
        || dm_string(&b.header.uuid)? != created.uuid
    {
        return Err(io::Errno::PROTO);
    }
    let mut probe = DmBuffer::request(312, 0, Some(&created.uuid), false)?;
    match dm_exchange(fd, DmCommand::Status, &mut probe) {
        Err(io::Errno::NXIO) => Ok(()),
        Err(error) => Err(error),
        Ok(()) => Err(io::Errno::BUSY),
    }
}

struct FreeLoop;
// SAFETY: This fixed Linux ioctl has no memory argument and returns its index
// in the syscall result; output_from_ptr checks the integer conversion.
#[allow(unsafe_code)]
unsafe impl ioctl::Ioctl for FreeLoop {
    type Output = u32;
    const IS_MUTATING: bool = false;
    fn opcode(&self) -> ioctl::Opcode {
        0x4c82
    }
    fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    // SAFETY: LOOP_CTL_GET_FREE takes no pointer and returns a nonnegative
    // loop index, checked by ioctl's errno path before this conversion.
    unsafe fn output_from_ptr(
        out: ioctl::IoctlOutput,
        _: *mut std::ffi::c_void,
    ) -> io::Result<u32> {
        u32::try_from(out).map_err(|_| io::Errno::PROTO)
    }
}

/// Query the loop control; this is not a reservation.
#[allow(unsafe_code)]
pub fn loop_free(fd: impl AsFd) -> io::Result<u32> {
    // SAFETY: fixed LOOP_CTL_GET_FREE, no pointer is dereferenced and AsFd
    // retains the live control FD for the synchronous call.
    unsafe { ioctl::ioctl(fd, FreeLoop) }
}

/// Bind the selected read-only file and verify its whole-file state on this FD.
/// EBUSY from SET_FD never triggers detach. Configuration failure permits
/// rollback only after a fresh exact readback; unknown or changed state is left
/// untouched. Separate kernel ioctls do not provide atomic compare-and-clear.
pub fn loop_attach(fd: impl AsFd, backing: impl AsFd) -> io::Result<()> {
    let file = rustix::fs::fstat(backing.as_fd())?;
    let device = rustix::fs::fstat(fd.as_fd())?;
    let expected = LoopStatus {
        backing_device: file.st_dev,
        inode: file.st_ino,
        offset: 0,
        size_limit: 0,
        number: rustix::fs::minor(device.st_rdev),
        flags: 1,
    };
    attach_loop_with(
        expected,
        || set_loop_fd(fd.as_fd(), backing.as_fd()),
        || set_loop_readonly(fd.as_fd()),
        || loop_status(fd.as_fd()),
        || clear_loop(fd.as_fd()),
    )
}

// The fixed operation seam keeps tests on the production control flow without
// exposing arbitrary ioctl requests or opening a host block device.
fn attach_loop_with(
    expected: LoopStatus,
    set_fd: impl FnOnce() -> io::Result<()>,
    set_readonly: impl FnOnce() -> io::Result<()>,
    status: impl FnOnce() -> io::Result<Option<LoopStatus>>,
    clear: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    set_fd()?;
    if let Err(error) = set_readonly() {
        // SET_FD is not a reservation: even this open read-only loop can have
        // its backing replaced. Only current exact ownership permits rollback.
        if status()? == Some(expected) {
            clear()?;
        }
        return Err(error);
    }
    // Failed readback is not evidence that the current association is ours.
    // In particular, never issue CLR_FD after an absent or unreadable status.
    if status()? != Some(expected) {
        return Err(io::Errno::PROTO);
    }
    Ok(())
}

#[allow(unsafe_code)]
fn set_loop_fd(fd: impl AsFd, backing: impl AsFd) -> io::Result<()> {
    // SAFETY: LOOP_SET_FD consumes the integer FD, not a pointer. Both AsFd
    // borrows keep descriptors live; only the kernel gains a file reference.
    unsafe {
        ioctl::ioctl(
            fd.as_fd(),
            IntegerSetter::<0x4c00>::new_usize(backing.as_fd().as_raw_fd() as usize),
        )
    }
}

#[allow(unsafe_code)]
fn set_loop_readonly(fd: impl AsFd) -> io::Result<()> {
    let mut value = LoopInfo {
        device: 0,
        inode: 0,
        rdevice: 0,
        offset: 0,
        size_limit: 0,
        number: 0,
        encryption: 0,
        key_size: 0,
        flags: 1,
        file_name: [0; 64],
        crypt_name: [0; 64],
        key: [0; 32],
        init: [0; 2],
    };
    // SAFETY: LOOP_SET_STATUS64 reads the initialized 232-byte Linux struct.
    // Updater conservatively allows writes, uniquely borrowing it through ioctl.
    unsafe { ioctl::ioctl(fd, Updater::<0x4c04, _>::new(&mut value)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const OWN_LOOP: LoopStatus = LoopStatus {
        backing_device: 2049,
        inode: 42,
        offset: 0,
        size_limit: 0,
        number: 3,
        flags: 1,
    };

    fn attach_trace(
        set_fd: io::Result<()>,
        set_readonly: io::Result<()>,
        status: io::Result<Option<LoopStatus>>,
        clear: io::Result<()>,
    ) -> (io::Result<()>, Vec<&'static str>) {
        let calls = RefCell::new(Vec::new());
        let result = attach_loop_with(
            OWN_LOOP,
            || {
                calls.borrow_mut().push("set_fd");
                set_fd
            },
            || {
                calls.borrow_mut().push("set_readonly");
                set_readonly
            },
            || {
                calls.borrow_mut().push("status");
                status
            },
            || {
                calls.borrow_mut().push("clear");
                clear
            },
        );
        (result, calls.into_inner())
    }

    fn unknown_loop_states() -> Vec<io::Result<Option<LoopStatus>>> {
        let mut states = vec![Ok(None), Err(io::Errno::IO), Err(io::Errno::BUSY)];
        for changed in [
            LoopStatus {
                backing_device: OWN_LOOP.backing_device + 1,
                ..OWN_LOOP
            },
            LoopStatus {
                inode: OWN_LOOP.inode + 1,
                ..OWN_LOOP
            },
            LoopStatus {
                number: OWN_LOOP.number + 1,
                ..OWN_LOOP
            },
            LoopStatus {
                flags: 0,
                ..OWN_LOOP
            },
            LoopStatus {
                flags: 5,
                ..OWN_LOOP
            },
            LoopStatus {
                offset: 4096,
                ..OWN_LOOP
            },
            LoopStatus {
                size_limit: 4096,
                ..OWN_LOOP
            },
        ] {
            states.push(Ok(Some(changed)));
        }
        states
    }

    #[test]
    fn loop_attach_refuses_changed_or_unknown_readback_without_clear() {
        for status in unknown_loop_states() {
            let (result, calls) = attach_trace(Ok(()), Ok(()), status, Ok(()));
            assert!(result.is_err(), "{status:?}");
            assert_eq!(calls, ["set_fd", "set_readonly", "status"], "{status:?}");
        }
    }

    #[test]
    fn loop_configure_error_does_not_clear_changed_or_unknown_binding() {
        for status in unknown_loop_states() {
            let (result, calls) = attach_trace(Ok(()), Err(io::Errno::INVAL), status, Ok(()));
            assert!(result.is_err(), "{status:?}");
            assert_eq!(calls, ["set_fd", "set_readonly", "status"], "{status:?}");
        }
    }

    #[test]
    fn loop_configure_error_clears_only_after_current_own_readback() {
        let (result, calls) =
            attach_trace(Ok(()), Err(io::Errno::INVAL), Ok(Some(OWN_LOOP)), Ok(()));
        assert_eq!(result, Err(io::Errno::INVAL));
        assert_eq!(calls, ["set_fd", "set_readonly", "status", "clear"]);
    }

    #[test]
    fn loop_owned_rollback_propagates_clear_failure() {
        let (result, calls) = attach_trace(
            Ok(()),
            Err(io::Errno::INVAL),
            Ok(Some(OWN_LOOP)),
            Err(io::Errno::IO),
        );
        assert_eq!(result, Err(io::Errno::IO));
        assert_eq!(calls, ["set_fd", "set_readonly", "status", "clear"]);
    }

    #[test]
    fn loop_set_fd_failure_never_configures_reads_or_clears() {
        for error in [io::Errno::BUSY, io::Errno::IO] {
            let (result, calls) = attach_trace(Err(error), Ok(()), Ok(Some(OWN_LOOP)), Ok(()));
            assert_eq!(result, Err(error));
            assert_eq!(calls, ["set_fd"]);
        }
    }

    #[test]
    fn loop_attach_accepts_exact_readonly_whole_file_without_clear() {
        let (result, calls) = attach_trace(Ok(()), Ok(()), Ok(Some(OWN_LOOP)), Ok(()));
        assert_eq!(result, Ok(()));
        assert_eq!(calls, ["set_fd", "set_readonly", "status"]);
    }

    #[test]
    fn load_is_bounded_readonly_single_verity_with_terminated_aligned_spec() {
        let created = DmCreated {
            device: 1,
            name: "mica-root".into(),
            uuid: "MICA-test".into(),
        };
        let mut target = DmTarget {
            sector: 0,
            length: 8,
            kind: "verity".into(),
            parameters: "1 7:0 7:0".into(),
        };
        let b = load_request(&created, &target).unwrap();
        assert_eq!(b.header.flags, DM_READONLY);
        assert_eq!(b.header.target_count, 1);
        assert_eq!(b.header.data_size, 368);
        assert_eq!(b.data[49], 0);
        for parameters in ["", "has\0nul", "has\nnewline"] {
            target.parameters = parameters.into();
            assert!(load_request(&created, &target).is_err());
        }
        target.parameters = "x".repeat(4097);
        assert!(load_request(&created, &target).is_err());
        target.parameters = "1".into();
        target.kind = "linear".into();
        assert!(load_request(&created, &target).is_err());
    }
}
