//! Bounded online acquisition and streaming offline component archives.
use crate::{
    catalog::{self, CatalogCheckpoint, CatalogRequest, SelectedRelease, VerifiedCatalog},
    components::{Artifact, Deployment, authenticate_deployment, component_id},
    deployments::{
        DeploymentStore, atomic_write, directory, read_bounded, sync_directory, verify_file,
    },
};
use anyhow::{Context, Result, ensure};
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Acquisition<'a> {
    pub root: PathBuf,
    pub store: &'a DeploymentStore,
    pub keys: &'a [[u8; 32]],
    pub board: &'a str,
    pub arch: &'a str,
    pub max_bytes: u64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyDeployment {
    pub id: String,
    pub path: PathBuf,
    pub objects: PathBuf,
    pub version: String,
    pub generation: u64,
}

impl Acquisition<'_> {
    pub fn objects(&self) -> PathBuf {
        self.root.join("verified/objects")
    }

    fn prepare(&self) -> Result<()> {
        ensure!(
            self.max_bytes > 0 && self.max_bytes <= 8 * 1024 * 1024 * 1024,
            "invalid workspace budget"
        );
        for path in [
            self.root.join("downloads"),
            self.objects(),
            self.root.join("staging"),
        ] {
            directory(&path)?;
        }
        Ok(())
    }

    fn files(&self) -> Result<Vec<(PathBuf, u64)>> {
        let mut stack = vec![(self.root.clone(), 0)];
        let mut files = Vec::new();
        let mut entries = 0;
        while let Some((path, depth)) = stack.pop() {
            ensure!(depth <= 3, "workspace directory depth exceeded");
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                entries += 1;
                ensure!(entries <= 4096, "workspace entry bound exceeded");
                let meta = entry.path().symlink_metadata()?;
                if meta.is_dir() {
                    stack.push((entry.path(), depth + 1));
                } else {
                    ensure!(meta.is_file(), "unexpected workspace file type");
                    files.push((entry.path(), meta.len()));
                }
            }
        }
        Ok(files)
    }

    fn reserve(&self, bytes: u64) -> Result<()> {
        let used = self
            .files()?
            .into_iter()
            .try_fold(0_u64, |sum, (_, bytes)| {
                sum.checked_add(bytes).context("workspace size overflow")
            })?;
        ensure!(
            used.checked_add(bytes)
                .is_some_and(|total| total <= self.max_bytes),
            "workspace budget exhausted"
        );
        let stat = rustix::fs::statvfs(&self.root)?;
        ensure!(
            stat.f_bavail.saturating_mul(stat.f_frsize) >= bytes.saturating_add(128 * 1024 * 1024)
                && (stat.f_files == 0 || stat.f_favail >= 2048 + 32),
            "DATA reserve unavailable"
        );
        Ok(())
    }

    /// Remove only the disposable acquisition workspace under the storage lock.
    pub fn discard(&self) -> Result<usize> {
        self.prepare()?;
        let files = self.files()?;
        for (path, _) in &files {
            fs::remove_file(path)?;
            sync_directory(path.parent().context("missing workspace parent")?)?;
        }
        Ok(files.len())
    }

    pub fn probe(&self) -> Result<serde_json::Value> {
        self.prepare()?;
        self.reserve(4096)?;
        let probe = self.root.join("staging/probe");
        atomic_write(&probe, b"workspace probe\n")?;
        fs::remove_file(&probe)?;
        sync_directory(probe.parent().context("missing probe parent")?)?;
        let stat = rustix::fs::statvfs(&self.root)?;
        Ok(serde_json::json!({
            "status":"ready", "root":self.root, "maxBytes":self.max_bytes,
            "freeBytes":stat.f_bavail.saturating_mul(stat.f_frsize),
            "freeInodes":stat.f_favail,
        }))
    }

    fn missing(&self, deployment: &Deployment) -> Result<BTreeMap<String, u64>> {
        catalog::artifacts(deployment)?;
        let mut missing = BTreeMap::new();
        for (path, artifact) in self.store.object_paths(deployment) {
            if path.symlink_metadata().is_ok() {
                verify_file(&path, artifact)?;
            } else {
                missing.insert(artifact.sha256.clone(), artifact.bytes);
            }
        }
        for (sha, bytes) in missing.clone() {
            let path = self.objects().join(&sha);
            if path.symlink_metadata().is_ok() {
                verify_file(
                    &path,
                    &Artifact {
                        sha256: sha.clone(),
                        bytes,
                    },
                )?;
                missing.remove(&sha);
            }
        }
        Ok(missing)
    }

    fn validate(&self, deployment: &Deployment) -> Result<()> {
        ensure!(
            deployment.board == self.board && deployment.arch == self.arch,
            "deployment targets another device"
        );
        let state = self.store.effective_state()?;
        ensure!(state.candidate.is_none(), "another deployment is pending");
        ensure!(
            deployment.generation > state.highest_generation,
            "deployment generation is not newer"
        );
        Ok(())
    }

    fn ready(&self, envelope: &[u8], deployment: Deployment) -> Result<ReadyDeployment> {
        ensure!(
            self.missing(&deployment)?.is_empty(),
            "deployment objects are incomplete"
        );
        let id = component_id(&serde_json::to_value(&deployment)?)?;
        let path = self.root.join(format!("verified/{id}.json"));
        atomic_write(&path, envelope)?;
        Ok(ReadyDeployment {
            id,
            path,
            objects: self.objects(),
            version: deployment.version,
            generation: deployment.generation,
        })
    }

    pub fn check(&self, source: &str, channel: &str, now: i64) -> Result<VerifiedCatalog> {
        catalog::source_url(source)?;
        self.prepare()?;
        self.reserve(catalog::MAX_CATALOG_ENVELOPE)?;
        let path = self.root.join("staging/catalog.partial");
        download(source, &path, catalog::MAX_CATALOG_ENVELOPE, false)?;
        let checkpoint_path = self.store.meta.join("catalog.json");
        let previous: Option<CatalogCheckpoint> = if checkpoint_path.symlink_metadata().is_ok() {
            Some(serde_json::from_slice(&read_bounded(
                &checkpoint_path,
                4096,
            )?)?)
        } else {
            None
        };
        let result = catalog::verify_catalog(
            &read_bounded(&path, catalog::MAX_CATALOG_ENVELOPE)?,
            self.keys,
            &CatalogRequest {
                source,
                board: self.board,
                arch: self.arch,
                channel,
                now,
                checkpoint: previous.as_ref(),
                highest_generation: self.store.effective_state()?.highest_generation,
            },
        )?;
        atomic_write(&checkpoint_path, &serde_json::to_vec(&result.checkpoint)?)?;
        fs::remove_file(&path)?;
        sync_directory(path.parent().context("missing catalog parent")?)?;
        Ok(result)
    }

    fn reserve_missing(&self, missing: &BTreeMap<String, u64>) -> Result<()> {
        let mut needed = 0_u64;
        for (sha, bytes) in missing {
            let partial = self.root.join(format!("downloads/{sha}.partial"));
            let held = if partial.symlink_metadata().is_ok() {
                let meta = partial.symlink_metadata()?;
                ensure!(
                    meta.is_file() && meta.len() <= *bytes,
                    "invalid partial object"
                );
                meta.len()
            } else {
                0
            };
            needed = needed
                .checked_add(bytes - held)
                .context("object size overflow")?;
        }
        self.reserve(needed + 24576)
    }

    pub fn fetch(&self, selected: SelectedRelease) -> Result<ReadyDeployment> {
        self.validate(&selected.deployment)?;
        self.prepare()?;
        let missing = self.missing(&selected.deployment)?;
        self.reserve_missing(&missing)?;
        for (sha, bytes) in missing {
            let object = selected
                .objects
                .iter()
                .find(|object| object.sha256 == sha && object.bytes == bytes)
                .context("missing acquisition URL")?;
            let partial = self.root.join(format!("downloads/{sha}.partial"));
            if !partial.try_exists()? || partial.metadata()?.len() < bytes {
                download(&object.url, &partial, bytes, true)?;
            }
            self.promote(&partial, &Artifact { sha256: sha, bytes })?;
        }
        self.ready(selected.envelope.as_bytes(), selected.deployment)
    }

    fn promote(&self, partial: &Path, artifact: &Artifact) -> Result<()> {
        if let Err(error) = verify_file(partial, artifact) {
            fs::remove_file(partial)?;
            sync_directory(partial.parent().context("missing object parent")?)?;
            return Err(error);
        }
        File::open(partial)?.sync_all()?;
        fs::rename(partial, self.objects().join(&artifact.sha256))?;
        sync_directory(&self.objects())?;
        sync_directory(partial.parent().context("missing object parent")?)
    }

    /// MOSUPD01: descriptor length (u32 BE), signed descriptor, object count
    /// (u32 BE), then digest (64 ASCII), size (u64 BE), and exact object bytes.
    /// There are no filenames, directory entries, links, compression or padding.
    pub fn import(&self, input: &mut impl Read) -> Result<ReadyDeployment> {
        let mut magic = [0; 8];
        input.read_exact(&mut magic)?;
        ensure!(&magic == b"MOSUPD01", "invalid component archive");
        let mut length = [0; 4];
        input.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        ensure!(
            length > 0 && length <= 24576,
            "archive descriptor exceeds bound"
        );
        let mut envelope = vec![0; length];
        input.read_exact(&mut envelope)?;
        let deployment = authenticate_deployment(&envelope, self.keys)?;
        self.validate(&deployment)?;
        let mut required = catalog::artifacts(&deployment)?;
        let mut count = [0; 4];
        input.read_exact(&mut count)?;
        ensure!(
            u32::from_be_bytes(count) as usize == required.len(),
            "archive object count mismatch"
        );
        self.prepare()?;
        let missing = self.missing(&deployment)?;
        self.reserve_missing(&missing)?;
        for _ in 0..required.len() {
            let mut sha = [0; 64];
            input.read_exact(&mut sha)?;
            let sha = String::from_utf8(sha.to_vec())?;
            let mut size = [0; 8];
            input.read_exact(&mut size)?;
            let size = u64::from_be_bytes(size);
            ensure!(
                required.remove(&sha) == Some(size),
                "unlisted, duplicate or wrong-sized archive object"
            );
            let partial = self.root.join(format!("downloads/{sha}.partial"));
            let mut output = if missing.contains_key(&sha) {
                if partial.symlink_metadata().is_ok() {
                    ensure!(
                        partial.symlink_metadata()?.is_file(),
                        "invalid partial object"
                    );
                    fs::remove_file(&partial)?;
                }
                Some(
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&partial)?,
                )
            } else {
                None
            };
            let mut remaining = size;
            let mut buffer = [0; 65536];
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            while remaining > 0 {
                let count = remaining.min(buffer.len() as u64) as usize;
                input.read_exact(&mut buffer[..count])?;
                hash.update(&buffer[..count]);
                if let Some(output) = &mut output {
                    output.write_all(&buffer[..count])?;
                }
                remaining -= count as u64;
            }
            ensure!(
                hex::encode(hash.finish()) == sha,
                "archive object digest mismatch"
            );
            if let Some(output) = output {
                output.sync_all()?;
                drop(output);
                self.promote(
                    &partial,
                    &Artifact {
                        sha256: sha,
                        bytes: size,
                    },
                )?;
            }
        }
        ensure!(input.read(&mut [0; 1])? == 0, "trailing archive bytes");
        self.ready(&envelope, deployment)
    }
}

/// Connection establishment bound, per resolved address.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Whole-transfer bound, checked before every socket read and write.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(1800);
/// A transfer averaging under this many bytes per second over one window is
/// abandoned; a socket silent for a whole window is abandoned at once.
const LOW_SPEED_BYTES: u64 = 1024;
const LOW_SPEED_WINDOW: Duration = Duration::from_secs(30);
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const MAX_RESPONSE_HEAD: usize = 16384;

/// Redirects followed before a transfer is refused.
const MAX_REDIRECTS: usize = 10;

/// One HTTP/1.1 GET into `path`, http or https only. Redirects are followed, up
/// to [`MAX_REDIRECTS`]: sources may sit behind a CDN, and every catalog and
/// object is authenticated by signature and digest rather than by where it was
/// served from. With `resume` a non-empty file continues through `Range` and
/// only `206` from exactly that offset is accepted; otherwise only `200` is.
fn download(url: &str, path: &Path, limit: u64, resume: bool) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        ensure!(
            path.symlink_metadata()?.is_file(),
            "invalid download target"
        );
    }
    let offset = if resume && path.symlink_metadata().is_ok() {
        path.metadata()?.len()
    } else {
        0
    };
    ensure!(offset <= limit, "transfer exceeded byte bound");
    let started = Instant::now();
    let mut url = url::Url::parse(url)?;
    for _ in 0..=MAX_REDIRECTS {
        match transfer_once(&url, path, offset, limit, started)? {
            None => return Ok(()),
            Some(location) => {
                url = url
                    .join(&location)
                    .context("invalid transfer redirect location")?
            }
        }
    }
    anyhow::bail!("component transfer redirected more than {MAX_REDIRECTS} times")
}

/// One request to `url`: `None` once the body is in `path`, or the location a
/// redirect names.
fn transfer_once(
    url: &url::Url,
    path: &Path,
    offset: u64,
    limit: u64,
    started: Instant,
) -> Result<Option<String>> {
    ensure!(
        ["http", "https"].contains(&url.scheme())
            && url.username().is_empty()
            && url.password().is_none(),
        "unsupported transfer URL"
    );
    let host = url.host().context("transfer URL has no host")?;
    let address = url
        .socket_addrs(|| None)?
        .into_iter()
        .find_map(|address| TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).ok())
        .context("component transfer could not connect")?;
    address.set_read_timeout(Some(LOW_SPEED_WINDOW))?;
    address.set_write_timeout(Some(LOW_SPEED_WINDOW))?;
    let socket = Paced {
        stream: address,
        started,
        window: (Instant::now(), 0),
    };
    let mut request = format!(
        "GET {}{} HTTP/1.1\r\nHost: {}{}\r\nUser-Agent: mica-deploy\r\nAccept: */*\r\n",
        url.path(),
        url.query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default(),
        host,
        url.port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default(),
    );
    if offset > 0 {
        request.push_str(&format!("Range: bytes={offset}-\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    let transfer = Transfer {
        request: &request,
        path,
        offset,
        limit,
    };
    if url.scheme() == "https" {
        let name = match host {
            url::Host::Domain(domain) => ServerName::try_from(domain.to_owned())?,
            url::Host::Ipv4(ip) => ServerName::from(std::net::IpAddr::V4(ip)),
            url::Host::Ipv6(ip) => ServerName::from(std::net::IpAddr::V6(ip)),
        };
        let connection = rustls::ClientConnection::new(tls_config()?, name)?;
        transfer.run(rustls::StreamOwned::new(connection, socket))
    } else {
        transfer.run(socket)
    }
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    let certificates = CertificateDer::pem_file_iter(CA_BUNDLE)
        .with_context(|| format!("read {CA_BUNDLE}"))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse {CA_BUNDLE}"))?;
    let (added, _) = roots.add_parsable_certificates(certificates);
    ensure!(added > 0, "no usable certificate authority in {CA_BUNDLE}");
    Ok(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ))
}

/// The socket beneath TLS and HTTP framing, so the transfer bounds hold for the
/// response head, chunk framing and body alike: the whole-transfer deadline is
/// checked before every read and write, and a window averaging under
/// [`LOW_SPEED_BYTES`] per second abandons the connection.
struct Paced {
    stream: TcpStream,
    started: Instant,
    window: (Instant, u64),
}

impl Paced {
    fn deadline(&self) -> std::io::Result<()> {
        if self.started.elapsed() < TRANSFER_TIMEOUT {
            Ok(())
        } else {
            Err(std::io::Error::other("component transfer timed out"))
        }
    }
}

impl Read for Paced {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.deadline()?;
        let count = self.stream.read(buffer)?;
        self.window.1 += count as u64;
        if count > 0 && self.window.0.elapsed() >= LOW_SPEED_WINDOW {
            if self.window.1 < LOW_SPEED_BYTES * LOW_SPEED_WINDOW.as_secs() {
                return Err(std::io::Error::other("component transfer too slow"));
            }
            self.window = (Instant::now(), 0);
        }
        Ok(count)
    }
}

impl Write for Paced {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.deadline()?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

struct Transfer<'a> {
    request: &'a str,
    path: &'a Path,
    offset: u64,
    limit: u64,
}

impl Transfer<'_> {
    fn run(&self, mut stream: impl Read + Write) -> Result<Option<String>> {
        stream.write_all(self.request.as_bytes())?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            ensure!(head.len() < MAX_RESPONSE_HEAD, "excessive response head");
            let mut byte = [0];
            reader
                .read_exact(&mut byte)
                .context("component transfer interrupted")?;
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).context("invalid response head")?;
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.strip_prefix("HTTP/1."))
            .and_then(|line| line.get(2..5))
            .context("invalid response status line")?;
        let mut length = None;
        let mut chunked = false;
        let mut range = None;
        let mut location = None;
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').context("invalid response header")?;
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = Some(value.parse::<u64>()?),
                "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
                "content-range" => range = Some(value.to_owned()),
                "location" => location = Some(value.to_owned()),
                _ => {}
            }
        }
        if ["301", "302", "303", "307", "308"].contains(&status) {
            return location
                .map(Some)
                .with_context(|| format!("transfer redirect {status} without a location"));
        }
        if self.offset > 0 {
            ensure!(
                status == "206"
                    && range
                        .is_some_and(|range| range.starts_with(&format!("bytes {}-", self.offset))),
                "unexpected transfer status {status}: the server did not resume"
            );
        } else {
            ensure!(status == "200", "unexpected transfer status {status}");
        }
        if !chunked && let Some(length) = length {
            ensure!(
                self.offset
                    .checked_add(length)
                    .is_some_and(|end| end <= self.limit),
                "transfer exceeded byte bound"
            );
        }
        let mut output = if self.offset > 0 {
            OpenOptions::new().append(true).open(self.path)?
        } else {
            File::create(self.path)?
        };
        let mut body: Box<dyn Read + '_> = if chunked {
            Box::new(Chunked {
                inner: &mut reader,
                remaining: 0,
                done: false,
            })
        } else if let Some(length) = length {
            Box::new((&mut reader).take(length))
        } else {
            Box::new(&mut reader)
        };
        let mut written = self.offset;
        let mut received = 0_u64;
        let mut buffer = vec![0; 65536];
        loop {
            let count = match body.read(&mut buffer) {
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    anyhow::bail!("component transfer stalled")
                }
                Err(error) => return Err(error).context("component transfer interrupted"),
            };
            if count == 0 {
                break;
            }
            written += count as u64;
            received += count as u64;
            ensure!(written <= self.limit, "transfer exceeded byte bound");
            output.write_all(&buffer[..count])?;
        }
        if !chunked && let Some(length) = length {
            ensure!(received == length, "component transfer interrupted");
        }
        Ok(None)
    }
}

/// `Transfer-Encoding: chunked`, trailers discarded.
struct Chunked<R> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    fn line(&mut self) -> std::io::Result<String> {
        let mut line = Vec::new();
        (&mut self.inner).take(1024).read_until(b'\n', &mut line)?;
        let line = String::from_utf8(line)
            .ok()
            .and_then(|line| line.strip_suffix("\r\n").map(str::to_owned))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk framing"))?;
        Ok(line)
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or_default().trim();
            self.remaining = u64::from_str_radix(size, 16)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk size"))?;
            if self.remaining == 0 {
                while !self.line()?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
        }
        let limit = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let count = self.inner.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= count as u64;
        if self.remaining == 0 && !self.line()?.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk framing",
            ));
        }
        Ok(count)
    }
}
