//! Acquisition freshness and exact component selection for the signed server catalog.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::components::{Deployment, authenticate_deployment, authenticate_payload, component_id};

pub const MAX_CATALOG_BYTES: usize = 1024 * 1024;
pub const MAX_CATALOG_ENVELOPE: u64 = (MAX_CATALOG_BYTES * 4 / 3 + 1024) as u64;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CatalogCheckpoint {
    pub source: String,
    pub revision: u64,
    pub payload_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Catalog {
    schema: String,
    revision: u64,
    issued_at: String,
    expires_at: String,
    channels: Vec<Head>,
    releases: Vec<Release>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Head {
    board: String,
    channel: String,
    release_id: String,
    generation: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    id: String,
    channel: String,
    notes: String,
    deployment: String,
    objects: Vec<SourceObject>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceObject {
    pub sha256: String,
    pub bytes: u64,
    pub url: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedRelease {
    pub deployment_id: String,
    pub deployment: Deployment,
    pub envelope: String,
    pub objects: Vec<SourceObject>,
    pub notes: String,
}
pub struct VerifiedCatalog {
    pub checkpoint: CatalogCheckpoint,
    pub selected: Option<SelectedRelease>,
}
pub struct CatalogRequest<'a> {
    pub source: &'a str,
    pub board: &'a str,
    pub arch: &'a str,
    pub channel: &'a str,
    pub now: i64,
    pub checkpoint: Option<&'a CatalogCheckpoint>,
    pub highest_generation: u64,
}

pub fn artifacts(deployment: &Deployment) -> Result<BTreeMap<String, u64>> {
    let mut objects = BTreeMap::new();
    for artifact in [
        &deployment.kernel.boot.artifact,
        &deployment.kernel.support.image,
        &deployment.kernel.support.signature,
        &deployment.rootfs.content.image,
        &deployment.rootfs.content.signature,
    ] {
        if let Some(previous) = objects.insert(artifact.sha256.clone(), artifact.bytes) {
            ensure!(previous == artifact.bytes, "conflicting object lengths");
        }
    }
    Ok(objects)
}

pub fn source_url(source: &str) -> Result<Url> {
    ensure!(source.len() <= 2048, "source URL too long");
    let url = Url::parse(source)?;
    ensure!(
        ["http", "https"].contains(&url.scheme())
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/v1/manifest.json",
        "invalid catalog source URL"
    );
    Ok(url)
}

pub fn verify_catalog(
    bytes: &[u8],
    keys: &[[u8; 32]],
    request: &CatalogRequest<'_>,
) -> Result<VerifiedCatalog> {
    let source = source_url(request.source)?;
    ensure!(
        ["stable", "beta", "dev"].contains(&request.channel),
        "invalid channel"
    );
    let payload = authenticate_payload(bytes, keys, MAX_CATALOG_BYTES)?;
    let raw: serde_json::Value = serde_json::from_slice(&payload)?;
    ensure!(serde_json::to_vec(&raw)? == payload, "noncanonical catalog");
    let catalog: Catalog = serde_json::from_value(raw)?;
    ensure!(
        catalog.schema == "mica/catalog/v1"
            && catalog.revision > 0
            && catalog.revision <= 9_007_199_254_740_991
            && catalog.releases.len() <= 128
            && catalog.channels.len() <= 12,
        "invalid catalog schema or bounds"
    );
    let issued = DateTime::parse_from_rfc3339(&catalog.issued_at)?.timestamp();
    let expires = DateTime::parse_from_rfc3339(&catalog.expires_at)?.timestamp();
    ensure!(
        issued <= request.now + 300
            && expires > request.now
            && expires > issued
            && expires - issued <= 30 * 86400,
        "catalog expired or clock is unsuitable"
    );
    let checkpoint = CatalogCheckpoint {
        source: request.source.to_owned(),
        revision: catalog.revision,
        payload_digest: hex::encode(ring::digest::digest(&ring::digest::SHA256, &payload)),
    };
    if let Some(previous) = request
        .checkpoint
        .filter(|previous| previous.source == request.source)
    {
        ensure!(
            checkpoint.revision >= previous.revision,
            "catalog revision rollback"
        );
        ensure!(
            checkpoint.revision != previous.revision
                || checkpoint.payload_digest == previous.payload_digest,
            "catalog revision changed its signed contents"
        );
    }
    let mut heads = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut generations = BTreeSet::new();
    let mut selected: Option<SelectedRelease> = None;
    for release in catalog.releases {
        ensure!(
            !release.id.is_empty()
                && release.id.len() <= 128
                && ids.insert(release.id.clone())
                && ["stable", "beta", "dev"].contains(&release.channel.as_str())
                && release.notes.chars().count() <= 10000,
            "invalid release identity"
        );
        let deployment = authenticate_deployment(release.deployment.as_bytes(), keys)?;
        let mut required = artifacts(&deployment)?;
        ensure!(
            release.objects.len() == required.len(),
            "missing or extra component objects"
        );
        for object in &release.objects {
            ensure!(
                required.remove(&object.sha256) == Some(object.bytes),
                "component object substitution"
            );
            let expected_url = source.join(&format!("/v1/objects/{}", object.sha256))?;
            ensure!(
                Url::parse(&object.url)? == expected_url,
                "object URL differs from the catalog origin or digest"
            );
        }
        ensure!(
            generations.insert((
                deployment.board.clone(),
                release.channel.clone(),
                deployment.generation
            )),
            "duplicate board/channel generation"
        );
        let head = heads
            .entry((deployment.board.clone(), release.channel.clone()))
            .or_insert((0, String::new()));
        if deployment.generation > head.0 {
            *head = (deployment.generation, release.id);
        }
        if deployment.board == request.board
            && deployment.arch == request.arch
            && release.channel == request.channel
            && deployment.generation > request.highest_generation
            && selected
                .as_ref()
                .is_none_or(|previous| deployment.generation > previous.deployment.generation)
        {
            selected = Some(SelectedRelease {
                deployment_id: component_id(&serde_json::to_value(&deployment)?)?,
                deployment,
                envelope: release.deployment,
                objects: release.objects,
                notes: release.notes,
            });
        }
    }
    ensure!(
        catalog.channels.len() == heads.len(),
        "missing or duplicate channel heads"
    );
    for head in catalog.channels {
        let expected = heads
            .remove(&(head.board, head.channel))
            .context("unknown channel head")?;
        ensure!(
            expected == (head.generation, head.release_id),
            "channel head is not its highest release"
        );
    }
    Ok(VerifiedCatalog {
        checkpoint,
        selected,
    })
}
