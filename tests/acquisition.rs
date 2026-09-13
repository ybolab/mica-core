use base64::{Engine, engine::general_purpose::STANDARD};
use mica_deploy::{
    acquisition::Acquisition, components::component_id, deployments::DeploymentStore,
};
use ring::{
    digest,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde_json::{Value, json};
use std::{fs, io::Cursor};
use tempfile::TempDir;

fn archive() -> (Vec<u8>, [u8; 32], String) {
    let bytes = vec![42_u8; 12288];
    let sha = hex::encode(digest::digest(&digest::SHA256, &bytes));
    let mut d: Value =
        serde_json::from_str(include_str!("component-contracts/deployment.json")).unwrap();
    for pointer in [
        "/kernel/boot/artifact",
        "/kernel/support/image",
        "/kernel/support/signature",
        "/rootfs/content/image",
        "/rootfs/content/signature",
    ] {
        *d.pointer_mut(pointer).unwrap() = json!({"bytes":bytes.len(),"sha256":sha});
    }
    d["kernel"]["id"] = component_id(&d["kernel"]).unwrap().into();
    d["rootfs"]["id"] = component_id(&d["rootfs"]).unwrap().into();
    let key = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
    let public: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    let payload = serde_json::to_vec(&d).unwrap();
    let envelope = format!(
        "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
        hex::encode(digest::digest(&digest::SHA256, &public)),
        STANDARD.encode(&payload),
        STANDARD.encode(key.sign(&payload).as_ref())
    );
    let mut archive = b"MOSUPD01".to_vec();
    archive.extend((envelope.len() as u32).to_be_bytes());
    archive.extend(envelope.as_bytes());
    archive.extend(1_u32.to_be_bytes());
    archive.extend(sha.as_bytes());
    archive.extend((bytes.len() as u64).to_be_bytes());
    archive.extend(bytes);
    (archive, public, sha)
}
fn fixture() -> (TempDir, DeploymentStore) {
    let dir = TempDir::new().unwrap();
    for name in ["system", "esp", "meta"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    fs::create_dir_all(dir.path().join("esp/loader/entries")).unwrap();
    let store = DeploymentStore::new(
        dir.path().join("system"),
        mica_deploy::deployments::BootBackend::Uefi {
            esp: dir.path().join("esp"),
        },
        dir.path().join("meta"),
    );
    (dir, store)
}

#[test]
fn offline_archive_authenticates_before_staging_and_never_publishes_partial_descriptors() {
    let (archive, key, sha) = archive();
    let (dir, store) = fixture();
    let keys = [key];
    let acq = Acquisition {
        root: dir.path().join("updates"),
        store: &store,
        keys: &keys,
        board: "x64",
        arch: "amd64",
        max_bytes: 1024 * 1024,
    };
    let ready = acq.import(&mut Cursor::new(&archive)).unwrap();
    assert!(ready.path.is_file());
    assert_eq!(fs::read(acq.objects().join(&sha)).unwrap(), vec![42; 12288]);
    assert!(store.state().unwrap().candidate.is_none());
    assert!(acq.import(&mut Cursor::new(&archive)).is_ok());
    let mut wrong = archive.clone();
    wrong[8..12].copy_from_slice(&24577_u32.to_be_bytes());
    assert!(acq.import(&mut Cursor::new(wrong)).is_err());
    let mut extra = archive.clone();
    extra.push(0);
    assert!(acq.import(&mut Cursor::new(extra)).is_err());
}

#[test]
fn interrupted_corrupt_oversized_and_foreign_archives_cannot_become_ready() {
    let (archive, key, _) = archive();
    for case in ["truncated", "digest", "budget", "board", "key", "symlink"] {
        let (dir, store) = fixture();
        let keys = [if case == "key" { [0; 32] } else { key }];
        let acq = Acquisition {
            root: dir.path().join("updates"),
            store: &store,
            keys: &keys,
            board: if case == "board" { "cx3576" } else { "x64" },
            arch: "amd64",
            max_bytes: if case == "budget" { 100 } else { 1024 * 1024 },
        };
        let mut input = archive.clone();
        if case == "truncated" {
            input.truncate(input.len() - 1);
        }
        if case == "digest" {
            *input.last_mut().unwrap() = 0;
        }
        if case == "symlink" {
            std::os::unix::fs::symlink(dir.path().join("meta"), &acq.root).unwrap();
        }
        assert!(acq.import(&mut Cursor::new(input)).is_err(), "{case}");
        let verified = acq.root.join("verified");
        if verified.is_dir() {
            assert!(!fs::read_dir(verified).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            }));
        }
    }
}

#[test]
fn one_existing_destination_does_not_hide_other_destinations_for_the_same_digest() {
    let (archive, key, sha) = archive();
    let (dir, store) = fixture();
    let keys = [key];
    let length = u32::from_be_bytes(archive[8..12].try_into().unwrap()) as usize;
    let deployment =
        mica_deploy::components::authenticate_deployment(&archive[12..12 + length], &keys).unwrap();
    let path = store.object_paths(&deployment)[0].0.clone();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![42; 12288]).unwrap();
    let acq = Acquisition {
        root: dir.path().join("updates"),
        store: &store,
        keys: &keys,
        board: "x64",
        arch: "amd64",
        max_bytes: 1024 * 1024,
    };
    acq.import(&mut Cursor::new(archive)).unwrap();
    assert!(acq.objects().join(sha).is_file());
}

#[test]
fn discard_is_bounded_to_acquisition_files_and_checks_all_paths_before_removing_any() {
    let (archive, key, _) = archive();
    let (dir, store) = fixture();
    let keys = [key];
    let acq = Acquisition {
        root: dir.path().join("updates"),
        store: &store,
        keys: &keys,
        board: "x64",
        arch: "amd64",
        max_bytes: 1024 * 1024,
    };
    let ready = acq.import(&mut Cursor::new(&archive)).unwrap();
    fs::write(store.meta.join("preserved"), b"metadata").unwrap();
    std::os::unix::fs::symlink(&store.meta, acq.root.join("downloads/link")).unwrap();
    assert!(acq.discard().is_err());
    assert!(ready.path.exists());
    fs::remove_file(acq.root.join("downloads/link")).unwrap();
    assert_eq!(acq.discard().unwrap(), 2);
    assert!(!ready.path.exists());
    assert_eq!(fs::read(store.meta.join("preserved")).unwrap(), b"metadata");
    assert_eq!(acq.discard().unwrap(), 0);
    assert!(acq.probe().is_ok());
    assert!(acq.import(&mut Cursor::new(&archive)).is_ok());
}

#[test]
fn online_catalog_and_resumed_objects_converge_on_the_offline_ready_format() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    let (archive, key, sha) = archive();
    let descriptor_length = u32::from_be_bytes(archive[8..12].try_into().unwrap()) as usize;
    let descriptor = String::from_utf8(archive[12..12 + descriptor_length].to_vec()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = format!("http://{}/v1/manifest.json", listener.local_addr().unwrap());
    let object_url = format!("http://{}/v1/objects/{sha}", listener.local_addr().unwrap());
    let payload = serde_json::to_vec(&json!({"schema":"mos/catalog/v1","revision":1,"issuedAt":"2026-09-09T00:00:00.000Z","expiresAt":"2026-09-10T00:00:00.000Z",
        "channels":[{"board":"x64","channel":"stable","releaseId":"test","generation":1}],
        "releases":[{"id":"test","channel":"stable","notes":"Resume test","deployment":descriptor,"objects":[{"sha256":sha,"bytes":12288,"url":object_url}]}]})).unwrap();
    let signer = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
    let catalog = format!(
        "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
        hex::encode(digest::digest(&digest::SHA256, &key)),
        STANDARD.encode(&payload),
        STANDARD.encode(signer.sign(&payload).as_ref())
    );
    let server = thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 8192);
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let request = String::from_utf8(header).unwrap();
            if request_number == 0 {
                assert!(request.starts_with("GET /v1/manifest.json "));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    catalog.len(),
                    catalog
                )
                .unwrap();
            } else {
                assert!(request.to_ascii_lowercase().contains("range: bytes=4096-"));
                stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8192\r\nContent-Range: bytes 4096-12287/12288\r\nConnection: close\r\n\r\n").unwrap();
                stream.write_all(&vec![42; 8192]).unwrap();
            }
        }
    });
    let (dir, store) = fixture();
    let keys = [key];
    let acq = Acquisition {
        root: dir.path().join("updates"),
        store: &store,
        keys: &keys,
        board: "x64",
        arch: "amd64",
        max_bytes: 2 * 1024 * 1024,
    };
    fs::create_dir_all(acq.root.join("downloads")).unwrap();
    fs::write(
        acq.root.join(format!("downloads/{sha}.partial")),
        vec![42; 4096],
    )
    .unwrap();
    let selected = acq
        .check(&source, "stable", 1788915600)
        .unwrap()
        .selected
        .unwrap();
    let ready = acq.fetch(selected).unwrap();
    server.join().unwrap();
    assert!(ready.path.is_file());
    assert_eq!(fs::read(acq.objects().join(&sha)).unwrap(), vec![42; 12288]);
    assert!(store.meta.join("catalog.json").is_file());
    assert!(!acq.root.join(format!("downloads/{sha}.partial")).exists());
}

/// Answers each accepted connection with the next raw response, holds the
/// connection open for the paired duration, and returns the request heads.
/// A connection that does not arrive within ten seconds ends the server, so a
/// client that failed before connecting cannot hang the test.
fn serve(
    listener: std::net::TcpListener,
    responses: Vec<(Vec<u8>, std::time::Duration)>,
) -> std::thread::JoinHandle<Vec<String>> {
    use std::io::{Read, Write};
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (response, hold) in responses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(_) => return requests,
                }
            };
            stream.set_nonblocking(false).unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            requests.push(String::from_utf8(header).unwrap());
            let _ = stream.write_all(&response);
            std::thread::sleep(hold);
        }
        requests
    })
}

fn signed_catalog(key: &[u8; 32], descriptor: &str, sha: &str, object_url: &str) -> String {
    let payload = serde_json::to_vec(&json!({"schema":"mos/catalog/v1","revision":1,"issuedAt":"2026-09-09T00:00:00.000Z","expiresAt":"2026-09-10T00:00:00.000Z",
        "channels":[{"board":"x64","channel":"stable","releaseId":"test","generation":1}],
        "releases":[{"id":"test","channel":"stable","notes":"Transfer test","deployment":descriptor,"objects":[{"sha256":sha,"bytes":12288,"url":object_url}]}]})).unwrap();
    let signer = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
    format!(
        "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
        hex::encode(digest::digest(&digest::SHA256, key)),
        STANDARD.encode(&payload),
        STANDARD.encode(signer.sign(&payload).as_ref())
    )
}

fn chunked(body: &[u8]) -> Vec<u8> {
    let mut response =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in body.chunks(1000) {
        response.extend(format!("{:x}\r\n", chunk.len()).as_bytes());
        response.extend(chunk);
        response.extend(b"\r\n");
    }
    response.extend(b"0\r\n\r\n");
    response
}

fn transfer_fixture(
    responses: impl FnOnce(&str) -> Vec<(Vec<u8>, std::time::Duration)>,
    partial: Option<usize>,
) -> (
    TempDir,
    DeploymentStore,
    [u8; 32],
    String,
    String,
    std::thread::JoinHandle<Vec<String>>,
) {
    let (archive, key, sha) = archive();
    let descriptor_length = u32::from_be_bytes(archive[8..12].try_into().unwrap()) as usize;
    let descriptor = String::from_utf8(archive[12..12 + descriptor_length].to_vec()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let catalog = signed_catalog(
        &key,
        &descriptor,
        &sha,
        &format!("http://{address}/v1/objects/{sha}"),
    );
    let server = serve(listener, responses(&catalog));
    let (dir, store) = fixture();
    let updates = dir.path().join("updates/downloads");
    fs::create_dir_all(&updates).unwrap();
    if let Some(bytes) = partial {
        fs::write(updates.join(format!("{sha}.partial")), vec![42; bytes]).unwrap();
    }
    (
        dir,
        store,
        key,
        sha,
        format!("http://{address}/v1/manifest.json"),
        server,
    )
}

fn acquisition<'a>(
    dir: &TempDir,
    store: &'a DeploymentStore,
    keys: &'a [[u8; 32]],
) -> Acquisition<'a> {
    Acquisition {
        root: dir.path().join("updates"),
        store,
        keys,
        board: "x64",
        arch: "amd64",
        max_bytes: 2 * 1024 * 1024,
    }
}

fn ok(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend(body);
    response
}

const NOW: i64 = 1788915600;
const NO_HOLD: std::time::Duration = std::time::Duration::ZERO;

#[test]
fn transfer_accepts_chunked_catalog_and_close_delimited_object() {
    let mut object = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
    object.extend(vec![42; 12288]);
    let (dir, store, key, sha, source, server) = transfer_fixture(
        |catalog| vec![(chunked(catalog.as_bytes()), NO_HOLD), (object, NO_HOLD)],
        None,
    );
    let keys = [key];
    let acq = acquisition(&dir, &store, &keys);
    let selected = acq.check(&source, "stable", NOW).unwrap().selected.unwrap();
    acq.fetch(selected).unwrap();
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /v1/manifest.json HTTP/1.1\r\n"));
    assert!(!requests[1].to_ascii_lowercase().contains("range:"));
    assert_eq!(fs::read(acq.objects().join(&sha)).unwrap(), vec![42; 12288]);
}

#[test]
fn transfer_refuses_errors_and_oversized_catalogs() {
    let limit = mica_deploy::catalog::MAX_CATALOG_ENVELOPE as usize;
    let mut unsized_body = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
    unsized_body.extend(vec![b' '; limit + 1]);
    for response in [
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            limit + 1
        )
        .into_bytes(),
        chunked(&vec![b' '; limit + 1]),
        unsized_body,
    ] {
        let (dir, store, key, _, source, server) =
            transfer_fixture(|_| vec![(response, NO_HOLD)], None);
        let keys = [key];
        assert!(
            acquisition(&dir, &store, &keys)
                .check(&source, "stable", NOW)
                .is_err()
        );
        server.join().unwrap();
    }
}

fn redirect(status: &str, location: Option<&str>) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
        location
            .map(|location| format!("Location: {location}\r\n"))
            .unwrap_or_default()
    )
    .into_bytes()
}

/// A CDN in front of the source: every object is signed, so where the bytes
/// come from is not what authenticates them, and redirects are followed.
#[test]
fn transfer_follows_redirects_and_resumes_at_the_new_location() {
    let (dir, store, key, sha, source, server) = transfer_fixture(
        |catalog| {
            vec![
                (redirect("302 Found", Some("/cdn/manifest.json")), NO_HOLD),
                (ok(catalog.as_bytes()), NO_HOLD),
                (redirect("307 Temporary Redirect", Some("../cdn/object")), NO_HOLD),
                (
                    [
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8192\r\nContent-Range: bytes 4096-12287/12288\r\nConnection: close\r\n\r\n".as_slice(),
                        &[42; 8192],
                    ]
                    .concat(),
                    NO_HOLD,
                ),
            ]
        },
        Some(4096),
    );
    let keys = [key];
    let acq = acquisition(&dir, &store, &keys);
    let selected = acq.check(&source, "stable", NOW).unwrap().selected.unwrap();
    acq.fetch(selected).unwrap();
    let requests = server.join().unwrap();
    assert!(
        requests[1].starts_with("GET /cdn/manifest.json HTTP/1.1\r\n"),
        "{}",
        requests[1]
    );
    assert!(
        requests[3].starts_with("GET /v1/cdn/object HTTP/1.1\r\n"),
        "{}",
        requests[3]
    );
    assert!(
        requests[3]
            .to_ascii_lowercase()
            .contains("range: bytes=4096-")
    );
    assert_eq!(fs::read(acq.objects().join(&sha)).unwrap(), vec![42; 12288]);
}

#[test]
fn transfer_refuses_endless_unlocated_and_non_http_redirects() {
    for (responses, reason) in [
        (
            vec![redirect("301 Moved Permanently", Some("/v1/manifest.json")); 11],
            "redirected more than 10 times",
        ),
        (vec![redirect("302 Found", None)], "without a location"),
        (
            vec![redirect(
                "308 Permanent Redirect",
                Some("ftp://127.0.0.1/v1/manifest.json"),
            )],
            "unsupported transfer URL",
        ),
        (
            vec![redirect(
                "302 Found",
                Some("http://user:secret@127.0.0.1/v1/manifest.json"),
            )],
            "unsupported transfer URL",
        ),
    ] {
        let (dir, store, key, _, source, server) = transfer_fixture(
            |_| responses.into_iter().map(|r| (r, NO_HOLD)).collect(),
            None,
        );
        let keys = [key];
        let Err(error) = acquisition(&dir, &store, &keys).check(&source, "stable", NOW) else {
            panic!("a bad redirect was accepted: {reason}");
        };
        assert!(format!("{error:#}").contains(reason), "{reason}: {error:#}");
        server.join().unwrap();
    }
}

#[test]
fn resumed_object_refuses_a_server_that_ignores_the_range() {
    let (dir, store, key, sha, source, server) = transfer_fixture(
        |catalog| {
            vec![
                (ok(catalog.as_bytes()), NO_HOLD),
                (ok(&vec![42; 12288]), NO_HOLD),
            ]
        },
        Some(4096),
    );
    let keys = [key];
    let acq = acquisition(&dir, &store, &keys);
    let selected = acq.check(&source, "stable", NOW).unwrap().selected.unwrap();
    assert!(acq.fetch(selected).is_err());
    assert!(
        server.join().unwrap()[1]
            .to_ascii_lowercase()
            .contains("range: bytes=4096-")
    );
    assert!(!acq.objects().join(&sha).exists());
}

#[test]
fn stalled_transfer_is_abandoned() {
    let started = std::time::Instant::now();
    let (dir, store, key, _, source, _server) = transfer_fixture(
        |_| {
            vec![(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{".to_vec(),
                std::time::Duration::from_secs(60),
            )]
        },
        None,
    );
    let keys = [key];
    assert!(
        acquisition(&dir, &store, &keys)
            .check(&source, "stable", NOW)
            .is_err()
    );
    let elapsed = started.elapsed().as_secs();
    assert!((29..45).contains(&elapsed), "abandoned after {elapsed}s");
}

#[test]
fn https_transfer_verifies_the_server_certificate() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let certificate =
        CertificateDer::from_pem_slice(include_bytes!("tls/untrusted-test-only.cert.pem")).unwrap();
    let key =
        PrivateKeyDer::from_pem_slice(include_bytes!("tls/untrusted-test-only.key.pem")).unwrap();
    let config = std::sync::Arc::new(
        rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap(),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let source = format!(
        "https://{}/v1/manifest.json",
        listener.local_addr().unwrap()
    );
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut connection = rustls::ServerConnection::new(config).unwrap();
        connection.complete_io(&mut stream).map(|_| ())
    });
    let (dir, store) = fixture();
    let keys = [[0; 32]];
    let Err(error) = acquisition(&dir, &store, &keys).check(&source, "stable", NOW) else {
        panic!("an untrusted certificate was accepted");
    };
    assert!(format!("{error:#}").contains("UnknownIssuer"), "{error:#}");
    assert!(server.join().unwrap().is_err());
}
