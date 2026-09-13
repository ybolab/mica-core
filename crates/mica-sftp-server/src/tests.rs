//! Protocol unit tests: requests built and replies decoded with russh-sftp's
//! own packet types, so the bytes on the wire are the ones a client sends.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use bytes::{Buf, Bytes};
use russh_sftp::protocol::{
    Close, FSetStat, FileAttributes, Fstat, Init, Lstat, MkDir, Open, OpenDir, OpenFlags, Packet,
    Read, ReadDir, ReadLink, RealPath, Remove, Rename, RmDir, SetStat, Stat, StatusCode, Symlink,
    Write,
};

use super::{MAX_PACKET_LENGTH, Session, serve};

/// Encode `packet` the way a client puts it on the wire: length, type, body.
fn wire(packet: Packet) -> Vec<u8> {
    Bytes::try_from(packet).expect("request encodes").to_vec()
}

/// One request through a session, one decoded reply back.
fn call(session: &mut Session, packet: Packet) -> Packet {
    let mut bytes = Bytes::from(wire(packet));
    bytes.advance(4);
    let reply = session.dispatch(bytes);
    let mut encoded = Bytes::try_from(reply).expect("reply encodes");
    encoded.advance(4);
    Packet::try_from(&mut encoded).expect("reply decodes")
}

fn status_of(packet: &Packet) -> StatusCode {
    match packet {
        Packet::Status(status) => status.status_code,
        other => panic!("expected a status reply, got {other:?}"),
    }
}

fn path(p: &Path) -> String {
    p.to_str().expect("temp paths are UTF-8").to_string()
}

fn open(session: &mut Session, id: u32, file: &Path, pflags: OpenFlags) -> Packet {
    call(
        session,
        Packet::Open(Open {
            id,
            filename: path(file),
            pflags,
            attrs: FileAttributes::default(),
        }),
    )
}

fn handle_of(packet: Packet) -> String {
    match packet {
        Packet::Handle(handle) => handle.handle,
        other => panic!("expected a handle, got {other:?}"),
    }
}

fn attrs_of(packet: Packet) -> FileAttributes {
    match packet {
        Packet::Attrs(attrs) => attrs.attrs,
        other => panic!("expected attributes, got {other:?}"),
    }
}

/// `S_IFMT` of the attributes' mode.
fn type_bits(attrs: &FileAttributes) -> u32 {
    attrs.permissions.expect("mode is always sent") & 0o170000
}

/// The request id a reply answers. `Packet::get_request_id` only knows
/// request types and reports 0 for every reply.
fn reply_id(packet: &Packet) -> u32 {
    match packet {
        Packet::Status(p) => p.id,
        Packet::Attrs(p) => p.id,
        Packet::Handle(p) => p.id,
        Packet::Data(p) => p.id,
        Packet::Name(p) => p.id,
        other => panic!("not a reply: {other:?}"),
    }
}

fn names_of(packet: Packet) -> Vec<russh_sftp::protocol::File> {
    match packet {
        Packet::Name(name) => name.files,
        other => panic!("expected names, got {other:?}"),
    }
}

/// True when this process is not held to file modes (root, or
/// CAP_DAC_OVERRIDE): probed rather than read from the uid, because a
/// capability grants the same thing.
fn modes_are_bypassed(dir: &Path) -> bool {
    let probe = dir.join("probe");
    std::fs::write(&probe, "x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let bypassed = std::fs::File::open(&probe).is_ok();
    std::fs::remove_file(&probe).unwrap();
    bypassed
}

#[test]
fn init_answers_version_3() {
    let mut session = Session::default();

    let reply = call(
        &mut session,
        Packet::Init(Init {
            version: 3,
            extensions: Default::default(),
        }),
    );

    match reply {
        Packet::Version(version) => assert_eq!(version.version, 3),
        other => panic!("expected a version reply, got {other:?}"),
    }
}

#[test]
fn a_written_file_reads_back_at_its_offsets_and_then_reports_eof() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("upload.bin");
    let mut session = Session::default();

    let handle = handle_of(open(
        &mut session,
        1,
        &file,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
    ));
    for (id, offset, data) in [(2, 0, &b"hello "[..]), (3, 6, &b"world"[..])] {
        let reply = call(
            &mut session,
            Packet::Write(Write {
                id,
                handle: handle.clone(),
                offset,
                data: data.to_vec(),
            }),
        );
        assert_eq!(status_of(&reply), StatusCode::Ok);
    }
    let closed = call(
        &mut session,
        Packet::Close(Close {
            id: 4,
            handle: handle.clone(),
        }),
    );
    assert_eq!(status_of(&closed), StatusCode::Ok);
    assert_eq!(std::fs::read(&file).unwrap(), b"hello world");

    let handle = handle_of(open(&mut session, 5, &file, OpenFlags::READ));
    let data = call(
        &mut session,
        Packet::Read(Read {
            id: 6,
            handle: handle.clone(),
            offset: 6,
            len: 32768,
        }),
    );
    match data {
        Packet::Data(data) => {
            assert_eq!(data.id, 6);
            assert_eq!(data.data, b"world");
        }
        other => panic!("expected data, got {other:?}"),
    }
    let eof = call(
        &mut session,
        Packet::Read(Read {
            id: 7,
            handle,
            offset: 11,
            len: 32768,
        }),
    );
    assert_eq!(status_of(&eof), StatusCode::Eof);
}

#[test]
fn a_read_at_the_end_of_the_offset_space_is_answered_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "x").unwrap();
    let mut session = Session::default();
    let handle = handle_of(open(&mut session, 1, &file, OpenFlags::READ));

    let reply = call(
        &mut session,
        Packet::Read(Read {
            id: 2,
            handle,
            offset: u64::MAX,
            len: 32768,
        }),
    );

    assert!(matches!(
        status_of(&reply),
        StatusCode::Eof | StatusCode::Failure
    ));
}

#[test]
fn a_closed_or_unknown_handle_is_a_failure_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "x").unwrap();
    let mut session = Session::default();
    let handle = handle_of(open(&mut session, 1, &file, OpenFlags::READ));
    call(
        &mut session,
        Packet::Close(Close {
            id: 2,
            handle: handle.clone(),
        }),
    );

    let reply = call(
        &mut session,
        Packet::Read(Read {
            id: 3,
            handle,
            offset: 0,
            len: 10,
        }),
    );

    assert_eq!(status_of(&reply), StatusCode::Failure);
}

#[test]
fn an_exclusive_create_of_an_existing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("exists");
    std::fs::write(&file, "keep").unwrap();
    let mut session = Session::default();

    let reply = open(
        &mut session,
        1,
        &file,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
    );

    assert_eq!(status_of(&reply), StatusCode::Failure);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
}

#[test]
fn a_create_takes_the_requested_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("private");
    let mut session = Session::default();

    let reply = call(
        &mut session,
        Packet::Open(Open {
            id: 1,
            filename: path(&file),
            pflags: OpenFlags::WRITE | OpenFlags::CREATE,
            attrs: FileAttributes {
                permissions: Some(0o600),
                ..FileAttributes::default()
            },
        }),
    );

    handle_of(reply);
    assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn a_missing_file_is_no_such_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::default();

    let reply = open(&mut session, 1, &dir.path().join("absent"), OpenFlags::READ);
    let stat = call(
        &mut session,
        Packet::Stat(Stat {
            id: 2,
            path: path(&dir.path().join("absent")),
        }),
    );

    assert_eq!(status_of(&reply), StatusCode::NoSuchFile);
    assert_eq!(status_of(&stat), StatusCode::NoSuchFile);
}

#[test]
fn a_mode_the_user_lacks_is_permission_denied() {
    let dir = tempfile::tempdir().unwrap();
    if modes_are_bypassed(dir.path()) {
        eprintln!("skipped: this process is not held to file modes (root or CAP_DAC_OVERRIDE)");
        return;
    }
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut session = Session::default();

    let reply = open(
        &mut session,
        1,
        &locked.join("new"),
        OpenFlags::WRITE | OpenFlags::CREATE,
    );

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(status_of(&reply), StatusCode::PermissionDenied);
}

#[test]
fn a_directory_lists_every_entry_with_attributes_then_reports_eof() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "12345").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let mut session = Session::default();

    let handle = handle_of(call(
        &mut session,
        Packet::OpenDir(OpenDir {
            id: 1,
            path: path(dir.path()),
        }),
    ));
    let mut names = Vec::new();
    let mut id = 2;
    loop {
        let reply = call(
            &mut session,
            Packet::ReadDir(ReadDir {
                id,
                handle: handle.clone(),
            }),
        );
        id += 1;
        match reply {
            Packet::Name(name) => names.extend(name.files),
            Packet::Status(status) => {
                assert_eq!(status.status_code, StatusCode::Eof);
                break;
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }
    names.sort_by(|a, b| a.filename.cmp(&b.filename));

    let listed: Vec<&str> = names.iter().map(|f| f.filename.as_str()).collect();
    assert_eq!(listed, ["file.txt", "sub"]);
    assert_eq!(names[0].attrs.size, Some(5));
    assert!(
        names[0].longname.starts_with("-rw"),
        "{}",
        names[0].longname
    );
    assert!(
        names[0].longname.ends_with(" file.txt"),
        "{}",
        names[0].longname
    );
    assert!(names[1].longname.starts_with('d'), "{}", names[1].longname);
    assert_eq!(type_bits(&names[1].attrs), 0o040000);
}

#[test]
fn stat_follows_a_symlink_and_lstat_does_not() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target"), "abc").unwrap();
    let mut session = Session::default();

    // OpenSSH's client sends the TARGET first and the link path second, and
    // OpenSSH's server honours that order; russh-sftp names the two fields the
    // other way round.
    let made = call(
        &mut session,
        Packet::Symlink(Symlink {
            id: 1,
            linkpath: "target".to_string(),
            targetpath: path(&dir.path().join("link")),
        }),
    );
    assert_eq!(status_of(&made), StatusCode::Ok);
    assert_eq!(
        std::fs::read_link(dir.path().join("link")).unwrap(),
        Path::new("target")
    );

    let link = path(&dir.path().join("link"));
    let followed = attrs_of(call(
        &mut session,
        Packet::Stat(Stat {
            id: 2,
            path: link.clone(),
        }),
    ));
    let not_followed = attrs_of(call(
        &mut session,
        Packet::Lstat(Lstat {
            id: 3,
            path: link.clone(),
        }),
    ));
    let target = names_of(call(
        &mut session,
        Packet::ReadLink(ReadLink { id: 4, path: link }),
    ));

    // By the file-type bits, not russh-sftp's `is_regular`/`is_symlink`,
    // which test bit containment and so call a symlink (0o120000) regular
    // (0o100000) too.
    assert_eq!(type_bits(&followed), 0o100000);
    assert_eq!(followed.size, Some(3));
    assert_eq!(type_bits(&not_followed), 0o120000);
    assert_eq!(target.len(), 1);
    assert_eq!(target[0].filename, "target");
}

#[test]
fn fstat_reports_the_open_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "four").unwrap();
    let mut session = Session::default();
    let handle = handle_of(open(&mut session, 1, &file, OpenFlags::READ));

    let attrs = attrs_of(call(&mut session, Packet::Fstat(Fstat { id: 2, handle })));

    assert_eq!(attrs.size, Some(4));
    assert_eq!(type_bits(&attrs), 0o100000);
}

#[test]
fn setstat_applies_permissions_and_times_and_fsetstat_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "0123456789").unwrap();
    let mut session = Session::default();

    let set = call(
        &mut session,
        Packet::SetStat(SetStat {
            id: 1,
            path: path(&file),
            attrs: FileAttributes {
                permissions: Some(0o640),
                atime: Some(1_000_000),
                mtime: Some(2_000_000),
                ..FileAttributes::default()
            },
        }),
    );
    assert_eq!(status_of(&set), StatusCode::Ok);
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o640);
    assert_eq!(meta.atime(), 1_000_000);
    assert_eq!(meta.mtime(), 2_000_000);

    let handle = handle_of(open(&mut session, 2, &file, OpenFlags::WRITE));
    let truncated = call(
        &mut session,
        Packet::FSetStat(FSetStat {
            id: 3,
            handle,
            attrs: FileAttributes {
                size: Some(4),
                ..FileAttributes::default()
            },
        }),
    );
    assert_eq!(status_of(&truncated), StatusCode::Ok);
    assert_eq!(std::fs::read(&file).unwrap(), b"0123");
}

#[test]
fn mkdir_rmdir_and_remove_change_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    let file = dir.path().join("gone");
    std::fs::write(&file, "x").unwrap();
    let mut session = Session::default();

    let made = call(
        &mut session,
        Packet::MkDir(MkDir {
            id: 1,
            path: path(&sub),
            attrs: FileAttributes {
                permissions: Some(0o750),
                ..FileAttributes::default()
            },
        }),
    );
    assert_eq!(status_of(&made), StatusCode::Ok);
    assert!(sub.is_dir());
    assert_eq!(std::fs::metadata(&sub).unwrap().mode() & 0o777, 0o750);

    let again = call(
        &mut session,
        Packet::MkDir(MkDir {
            id: 2,
            path: path(&sub),
            attrs: FileAttributes::default(),
        }),
    );
    assert_eq!(status_of(&again), StatusCode::Failure);

    let removed_dir = call(
        &mut session,
        Packet::RmDir(RmDir {
            id: 3,
            path: path(&sub),
        }),
    );
    let removed_file = call(
        &mut session,
        Packet::Remove(Remove {
            id: 4,
            filename: path(&file),
        }),
    );
    assert_eq!(status_of(&removed_dir), StatusCode::Ok);
    assert_eq!(status_of(&removed_file), StatusCode::Ok);
    assert!(!sub.exists());
    assert!(!file.exists());
}

#[test]
fn rename_moves_a_file_and_refuses_to_replace_an_existing_one() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    let new = dir.path().join("new");
    let taken = dir.path().join("taken");
    std::fs::write(&old, "moved").unwrap();
    std::fs::write(&taken, "keep").unwrap();
    let mut session = Session::default();

    let moved = call(
        &mut session,
        Packet::Rename(Rename {
            id: 1,
            oldpath: path(&old),
            newpath: path(&new),
        }),
    );
    let refused = call(
        &mut session,
        Packet::Rename(Rename {
            id: 2,
            oldpath: path(&new),
            newpath: path(&taken),
        }),
    );

    assert_eq!(status_of(&moved), StatusCode::Ok);
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "moved");
    assert!(!old.exists());
    assert_eq!(status_of(&refused), StatusCode::Failure);
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "keep");
    assert!(new.exists());
}

#[test]
fn realpath_resolves_dot_symlinks_and_a_missing_last_component() {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(real.join("sub")).unwrap();
    std::os::unix::fs::symlink(real.join("sub"), real.join("via")).unwrap();
    let mut session = Session::default();

    let resolve = |session: &mut Session, id: u32, p: String| {
        let files = names_of(call(session, Packet::RealPath(RealPath { id, path: p })));
        assert_eq!(files.len(), 1);
        files[0].filename.clone()
    };

    assert_eq!(
        resolve(&mut session, 1, ".".to_string()),
        path(&std::env::current_dir().unwrap())
    );
    assert_eq!(
        resolve(&mut session, 2, String::new()),
        path(&std::env::current_dir().unwrap())
    );
    assert_eq!(
        resolve(&mut session, 3, format!("{}/via/../sub/.", path(&real))),
        path(&real.join("sub"))
    );
    assert_eq!(
        resolve(&mut session, 4, format!("{}/via/new-file", path(&real))),
        path(&real.join("sub").join("new-file"))
    );
}

#[test]
fn an_unknown_packet_type_is_unsupported_under_its_own_request_id() {
    let mut session = Session::default();
    // Type 99, request id 42, no body.
    let body = Bytes::from_static(&[99, 0, 0, 0, 42]);

    let mut encoded = Bytes::try_from(session.dispatch(body)).unwrap();
    encoded.advance(4);
    let reply = Packet::try_from(&mut encoded).unwrap();

    assert_eq!(status_of(&reply), StatusCode::OpUnsupported);
    assert_eq!(reply_id(&reply), 42);
}

#[test]
fn serve_answers_each_request_in_order_and_ends_cleanly_at_eof() {
    let dir = tempfile::tempdir().unwrap();
    let mut input = wire(Packet::Init(Init {
        version: 3,
        extensions: Default::default(),
    }));
    input.extend(wire(Packet::Stat(Stat {
        id: 7,
        path: path(dir.path()),
    })));
    let mut output = Vec::new();

    serve(input.as_slice(), &mut output).expect("EOF after whole packets is a clean end");

    let mut replies = Bytes::from(output);
    let mut decoded = Vec::new();
    while replies.has_remaining() {
        let length = replies.get_u32() as usize;
        let mut one = replies.split_to(length);
        decoded.push(Packet::try_from(&mut one).unwrap());
    }
    assert_eq!(decoded.len(), 2);
    assert!(matches!(decoded[0], Packet::Version(_)));
    assert_eq!(reply_id(&decoded[1]), 7);
    assert!(matches!(decoded[1], Packet::Attrs(_)));
}

#[test]
fn serve_refuses_a_packet_longer_than_the_limit() {
    let input = (MAX_PACKET_LENGTH + 1).to_be_bytes();
    let mut output = Vec::new();

    let err = serve(&input[..], &mut output).expect_err("an oversized packet ends the session");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(output.is_empty());
}
