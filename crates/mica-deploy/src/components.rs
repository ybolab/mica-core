//! Signed component contracts shared by publication, installation and early boot.

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{digest, signature};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_DEPLOYMENT_BYTES: usize = 16384;
const MAX_ENVELOPE_BYTES: usize = 24576;
const MAX_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug)]
pub struct ContractError(&'static str);

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid component contract: {}", self.0)
    }
}
impl std::error::Error for ContractError {}
type Result<T> = std::result::Result<T, ContractError>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VerityGeometry {
    pub version: u64,
    pub algorithm: String,
    pub data_block_size: u64,
    pub hash_block_size: u64,
    pub data_blocks: u64,
    pub hash_offset: u64,
    pub salt: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VerityImage {
    pub image: Artifact,
    pub root_hash: String,
    pub signature: Artifact,
    pub verity: VerityGeometry,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootArtifact {
    pub format: String,
    pub artifact: Artifact,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KernelComponent {
    pub schema: String,
    pub id: String,
    pub board: String,
    pub arch: String,
    pub build_id: String,
    pub release: String,
    pub boot: BootArtifact,
    pub support: VerityImage,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootComponent {
    pub schema: String,
    pub id: String,
    pub arch: String,
    pub version: String,
    pub content: VerityImage,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Deployment {
    pub schema: String,
    pub board: String,
    pub arch: String,
    pub generation: u64,
    pub version: String,
    pub data_policy: String,
    pub kernel: KernelComponent,
    pub rootfs: RootComponent,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BootIdentity {
    pub board: String,
    pub arch: String,
    pub kernel_build_id: String,
    pub kernel_release: String,
    pub support_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Envelope {
    schema: String,
    key_id: String,
    payload: String,
    signature: String,
}

#[derive(Debug)]
pub struct DeploymentPaths {
    pub rootfs: String,
    pub support: String,
    pub boot: String,
}

fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ContractError(message))
    }
}

fn hash(value: &str) -> Result<()> {
    require(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid digest",
    )
}

fn name(value: &str) -> Result<()> {
    require(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)),
        "invalid identifier",
    )
}

fn integer(value: u64, maximum: u64) -> Result<()> {
    require(value > 0 && value <= maximum, "invalid integer")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes))
}

/// Components exclude their own ID; deployments contain no self-reference.
pub fn component_id(value: &Value) -> Result<String> {
    let mut fields = value
        .as_object()
        .ok_or(ContractError("expected object"))?
        .clone();
    fields.remove("id");
    let bytes = serde_json::to_vec(&fields).map_err(|_| ContractError("invalid JSON"))?;
    Ok(sha256(&bytes))
}

impl Artifact {
    fn validate(&self, maximum: u64) -> Result<()> {
        integer(self.bytes, maximum)?;
        hash(&self.sha256)
    }

    /// Check downloaded or published bytes; boot uses signed dm-verity instead.
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        self.validate(MAX_INTEGER)?;
        require(
            bytes.len() as u64 == self.bytes && sha256(bytes) == self.sha256,
            "artifact length or digest mismatch",
        )
    }
}

impl VerityImage {
    fn validate(&self) -> Result<()> {
        self.image.validate(MAX_INTEGER)?;
        self.signature.validate(65536)?;
        hash(&self.root_hash)?;
        let g = &self.verity;
        require(
            g.version == 1
                && g.algorithm == "sha256"
                && g.data_block_size == 4096
                && g.hash_block_size == 4096,
            "unsupported verity geometry",
        )?;
        integer(g.data_blocks, MAX_INTEGER / 4096)?;
        integer(g.hash_offset, MAX_INTEGER)?;
        hash(&g.salt)?;
        require(
            g.hash_offset == g.data_blocks * 4096,
            "hash tree must immediately follow data",
        )?;
        let mut blocks = g.data_blocks;
        let mut tree_blocks = 0;
        while blocks > 1 {
            blocks = blocks.div_ceil(128);
            tree_blocks += blocks;
        }
        let bytes = g.hash_offset + tree_blocks * 4096;
        integer(bytes, MAX_INTEGER)?;
        require(
            self.image.bytes == bytes,
            "image length does not match verity tree",
        )
    }
}

impl Deployment {
    fn validate(&self, raw: &Value) -> Result<()> {
        require(
            self.schema == "mos/deployment/v1" && self.data_policy == "unchanged",
            "unsupported deployment schema or DATA policy",
        )?;
        let (arch, format) = match self.board.as_str() {
            "x64" => ("amd64", "uki"),
            "virt-arm64" => ("arm64", "uki"),
            "cx3576" | "s905x5m" => ("arm64", "fit"),
            _ => return Err(ContractError("unsupported board")),
        };
        require(self.arch == arch, "board/architecture mismatch")?;
        integer(self.generation, MAX_INTEGER)?;
        name(&self.version)?;
        let k = &self.kernel;
        let r = &self.rootfs;
        require(
            k.schema == "mos/kernel/v1" && r.schema == "mos/rootfs/v1",
            "wrong component schema",
        )?;
        require(
            k.board == self.board && k.arch == self.arch && r.arch == self.arch,
            "component target mismatch",
        )?;
        hash(&k.id)?;
        hash(&r.id)?;
        hash(&k.build_id)?;
        name(&k.release)?;
        name(&r.version)?;
        require(k.boot.format == format, "wrong boot format")?;
        k.boot.artifact.validate(MAX_INTEGER)?;
        k.support.validate()?;
        r.content.validate()?;
        require(
            component_id(&raw["kernel"])? == k.id && component_id(&raw["rootfs"])? == r.id,
            "component identity mismatch",
        )
    }

    /// Paths are derived only from the IDs of a validated deployment.
    pub fn paths(&self) -> Result<DeploymentPaths> {
        let raw = serde_json::to_value(self).map_err(|_| ContractError("invalid JSON"))?;
        self.validate(&raw)?;
        Ok(DeploymentPaths {
            rootfs: format!("roots/{}/rootfs.img", self.rootfs.id),
            support: format!("kernels/{}/support.img", self.kernel.id),
            boot: if self.kernel.boot.format == "uki" {
                format!("EFI/mica/kernels/{}.efi", self.kernel.id)
            } else {
                format!("kernels/{}/boot.itb", self.kernel.id)
            },
        })
    }
}

/// Parse the compact, key-sorted payload with a strict size and schema boundary.
pub fn parse_deployment(payload: &[u8]) -> Result<Deployment> {
    require(
        payload.len() <= MAX_DEPLOYMENT_BYTES,
        "deployment too large",
    )?;
    let raw: Value = serde_json::from_slice(payload).map_err(|_| ContractError("invalid JSON"))?;
    let canonical = serde_json::to_vec(&raw).map_err(|_| ContractError("invalid JSON"))?;
    require(
        canonical == payload,
        "noncanonical or duplicate JSON fields",
    )?;
    let descriptor: Deployment = serde_json::from_value(raw.clone())
        .map_err(|_| ContractError("unknown, missing or invalid fields"))?;
    descriptor.validate(&raw)?;
    Ok(descriptor)
}

fn base64(value: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| ContractError("invalid base64"))?;
    require(STANDARD.encode(&bytes) == value, "noncanonical base64")?;
    Ok(bytes)
}

/// Authenticate acquisition metadata before choosing an exact tested combination.
pub fn authenticate_payload(
    bytes: &[u8],
    public_keys: &[[u8; 32]],
    limit: usize,
) -> Result<Vec<u8>> {
    require(bytes.len() <= limit * 4 / 3 + 1024, "envelope too large")?;
    let e: Envelope =
        serde_json::from_slice(bytes).map_err(|_| ContractError("invalid envelope"))?;
    require(
        serde_json::to_vec(&e).map_err(|_| ContractError("invalid envelope"))? == bytes,
        "noncanonical envelope",
    )?;
    require(
        e.schema == "mos/update-envelope/v1",
        "wrong envelope schema",
    )?;
    hash(&e.key_id)?;
    require(
        !public_keys.is_empty() && public_keys.len() <= 8,
        "invalid trust set",
    )?;
    let key = public_keys
        .iter()
        .find(|key| sha256(*key) == e.key_id)
        .ok_or(ContractError("untrusted metadata key"))?;
    let payload = base64(&e.payload)?;
    require(payload.len() <= limit, "payload too large")?;
    let signature = base64(&e.signature)?;
    require(signature.len() == 64, "invalid signature length")?;
    signature::UnparsedPublicKey::new(&signature::ED25519, key)
        .verify(&payload, &signature)
        .map_err(|_| ContractError("metadata signature rejected"))?;
    Ok(payload)
}

pub fn authenticate_deployment(bytes: &[u8], public_keys: &[[u8; 32]]) -> Result<Deployment> {
    require(bytes.len() <= MAX_ENVELOPE_BYTES, "envelope too large")?;
    parse_deployment(&authenticate_payload(
        bytes,
        public_keys,
        MAX_DEPLOYMENT_BYTES,
    )?)
}

/// Boot additionally binds the signed deployment to the running UKI/FIT.
pub fn verify_deployment(
    bytes: &[u8],
    public_keys: &[[u8; 32]],
    running: &BootIdentity,
) -> Result<Deployment> {
    let d = authenticate_deployment(bytes, public_keys)?;
    require(
        d.board == running.board
            && d.arch == running.arch
            && d.kernel.build_id == running.kernel_build_id
            && d.kernel.release == running.kernel_release
            && component_id(
                &serde_json::to_value(&d.kernel.support)
                    .map_err(|_| ContractError("invalid support metadata"))?,
            )? == running.support_id,
        "running kernel/support mismatch",
    )?;
    Ok(d)
}
