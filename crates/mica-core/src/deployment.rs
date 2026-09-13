//! Native signed deployment status and actions through mica-deploy.
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::update_lifecycle::UpdateClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootBackend {
    Uefi,
    UbootFit,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Boot {
    pub backend: BootBackend,
    pub deployment_id: String,
    pub entry: String,
    pub kernel_id: String,
    pub rootfs_id: String,
    pub content_verified: bool,
    pub secure_boot: bool,
    pub boot_verified: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct State {
    pub highest_generation: u64,
    pub current: Option<String>,
    pub fallback: Option<String>,
    pub candidate: Option<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Deployment {
    pub id: String,
    pub file: String,
    pub generation: u64,
    pub tries_left: Option<u8>,
    pub version: String,
    pub kernel_id: String,
    pub kernel_release: String,
    pub rootfs_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub boot: Boot,
    pub state: State,
    pub deployments: Vec<Deployment>,
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Status {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() <= 65536, "deployment status exceeds bound");
        let status: Self = serde_json::from_str(text)?;
        ensure!(
            status.deployments.len() <= 16 && status.state.failed.len() <= 128,
            "deployment count exceeds bound"
        );
        for (index, deployment) in status.deployments.iter().enumerate() {
            ensure!(
                valid_id(&deployment.id)
                    && valid_id(&deployment.kernel_id)
                    && valid_id(&deployment.rootfs_id)
                    && deployment.generation > 0
                    && deployment.tries_left.is_none_or(|tries| tries <= 3),
                "invalid deployment identity"
            );
            ensure!(
                !status.deployments[..index]
                    .iter()
                    .any(|other| other.id == deployment.id
                        || other.generation == deployment.generation),
                "ambiguous deployment status"
            );
        }
        let booted = status.booted().context("running deployment is absent")?;
        match status.boot.backend {
            BootBackend::Uefi => {
                ensure!(
                    ["", "+3", "+2-1", "+1-2", "+0-3"].iter().any(|suffix| {
                        status.boot.entry
                            == format!("mica-{}{suffix}.conf", status.boot.deployment_id)
                    }),
                    "running boot entry identity mismatch"
                );
                ensure!(
                    status.boot.boot_verified == status.boot.secure_boot,
                    "inconsistent UEFI verification evidence"
                );
            }
            BootBackend::UbootFit => {
                ensure!(
                    status.boot.entry == format!("fit:{}", status.boot.deployment_id),
                    "running FIT entry identity mismatch"
                );
                ensure!(
                    status.boot.boot_verified && !status.boot.secure_boot,
                    "inconsistent FIT verification evidence"
                );
            }
        }
        ensure!(
            status.boot.content_verified
                && booted.kernel_id == status.boot.kernel_id
                && booted.rootfs_id == status.boot.rootfs_id,
            "running component identity mismatch"
        );
        for id in [
            &status.state.current,
            &status.state.fallback,
            &status.state.candidate,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                status
                    .deployments
                    .iter()
                    .any(|deployment| &deployment.id == id),
                "state references an absent deployment"
            );
        }
        ensure!(
            status.state.failed.iter().all(|id| valid_id(id)),
            "invalid failed deployment ID"
        );
        Ok(status)
    }

    pub fn booted(&self) -> Option<&Deployment> {
        self.deployments
            .iter()
            .find(|deployment| deployment.id == self.boot.deployment_id)
    }

    pub fn phase(&self) -> (&'static str, String) {
        let running = &self.boot.deployment_id;
        if self.state.failed.contains(running) {
            return (
                "reboot-required",
                "The running deployment was rejected; reboot to the retained fallback".into(),
            );
        }
        if let Some(candidate) = &self.state.candidate {
            if candidate != running {
                return (
                    "reboot-required",
                    format!("Deployment {candidate} is installed and awaits reboot"),
                );
            }
            return (
                "validating",
                "The running candidate awaits health confirmation".into(),
            );
        }
        if self.state.current.as_ref() != Some(running) {
            return (
                "validating",
                "The running deployment awaits health confirmation".into(),
            );
        }
        if self
            .booted()
            .is_some_and(|deployment| deployment.generation < self.state.highest_generation)
            && !self.state.failed.is_empty()
        {
            return (
                "rolled-back",
                "The system is running a retained deployment after a failed update".into(),
            );
        }
        (
            "succeeded",
            "The boot health gate confirmed this deployment".into(),
        )
    }

    pub fn rollback(&self) -> Value {
        let reason = if self.state.candidate.is_some() {
            Some("candidate_pending")
        } else if self.state.current.as_ref() != Some(&self.boot.deployment_id)
            || self.state.failed.contains(&self.boot.deployment_id)
        {
            Some("running_not_confirmed")
        } else if self.state.fallback.as_ref().is_none_or(|id| {
            id == &self.boot.deployment_id
                || self.state.failed.contains(id)
                || !self
                    .deployments
                    .iter()
                    .any(|deployment| &deployment.id == id && deployment.tries_left != Some(0))
        }) {
            Some("no_usable_fallback")
        } else {
            None
        };
        json!({"permitted":reason.is_none(), "target":if reason.is_none() { self.state.fallback.as_ref() } else { None }, "reason":reason})
    }

    pub fn merge_into(&self, entry: &mut serde_json::Map<String, Value>) -> Result<()> {
        for (key, value) in serde_json::to_value(self)?
            .as_object()
            .context("invalid status object")?
        {
            entry.insert(key.clone(), value.clone());
        }
        entry.insert("rollback".into(), self.rollback());
        Ok(())
    }
}

#[async_trait::async_trait]
pub trait DeploymentClient: Send + Sync {
    async fn status(&self) -> Result<Status>;
    async fn install(&self, descriptor: &Path) -> Result<()>;
    async fn confirm(&self) -> Result<()>;
    async fn reject(&self, id: &str) -> Result<()>;
    async fn rollback(&self) -> Result<()>;
}

pub struct NativeClient {
    client: Arc<dyn UpdateClient>,
}
impl NativeClient {
    pub fn new(client: Arc<dyn UpdateClient>) -> Self {
        Self { client }
    }

    async fn action(&self, args: Vec<String>, timeout: Duration) -> Result<String> {
        let output = self.client.run(&args, timeout).await?;
        ensure!(
            output.code == Some(0),
            "mica-deploy {}: {}",
            args[0],
            output.stderr.trim()
        );
        ensure!(
            output.stdout.len() <= 65536,
            "deployment output exceeds bound"
        );
        Ok(output.stdout)
    }
}

#[async_trait::async_trait]
impl DeploymentClient for NativeClient {
    async fn status(&self) -> Result<Status> {
        Status::parse(
            &self
                .action(vec!["status".into()], Duration::from_secs(30))
                .await?,
        )
    }
    async fn install(&self, descriptor: &Path) -> Result<()> {
        let objects = descriptor
            .parent()
            .context("missing descriptor directory")?
            .join("objects");
        self.action(
            vec![
                "install".into(),
                descriptor.to_string_lossy().into_owned(),
                "--objects".into(),
                objects.to_string_lossy().into_owned(),
            ],
            Duration::from_secs(1800),
        )
        .await?;
        Ok(())
    }
    async fn confirm(&self) -> Result<()> {
        self.action(vec!["confirm".into()], Duration::from_secs(30))
            .await?;
        Ok(())
    }
    async fn reject(&self, id: &str) -> Result<()> {
        ensure!(valid_id(id), "invalid deployment ID");
        self.action(vec!["reject".into(), id.into()], Duration::from_secs(30))
            .await?;
        Ok(())
    }
    async fn rollback(&self) -> Result<()> {
        self.action(vec!["rollback".into()], Duration::from_secs(30))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fixture() -> Value {
        let current = "a".repeat(64);
        let fallback = "b".repeat(64);
        let kernel = "c".repeat(64);
        let root = "d".repeat(64);
        let deployments = [(&current, 2), (&fallback, 1)].map(|(id, generation)| json!({
            "id":id,"file":format!("mica-{id}.conf"),"generation":generation,"triesLeft":null,
            "version":format!("test-{generation}"),"kernelId":kernel,"kernelRelease":"6.12.107","rootfsId":root,
        }));
        json!({"boot":{"deploymentId":current,"entry":format!("mica-{current}.conf"),"kernelId":kernel,
            "rootfsId":root,"contentVerified":true,"secureBoot":true,"bootVerified":true,"backend":"uefi"},
            "state":{"highestGeneration":2,"current":current,"fallback":fallback,"candidate":null,"failed":[]},"deployments":deployments})
    }

    #[test]
    fn native_confirmation_and_trial_states_drive_the_product_status() {
        let mut value = fixture();
        let status = Status::parse(&value.to_string()).unwrap();
        assert_eq!(status.phase().0, "succeeded");
        assert_eq!(status.rollback()["target"], "b".repeat(64));
        let mut entry = serde_json::Map::from_iter([("install".into(), json!({"status":"done"}))]);
        status.merge_into(&mut entry).unwrap();
        assert_eq!(entry["install"]["status"], "done");
        value["state"]["candidate"] = value["boot"]["deploymentId"].clone();
        value["state"]["current"] = value["state"]["fallback"].clone();
        value["state"]["fallback"] = Value::Null;
        value["deployments"][0]["triesLeft"] = json!(2);
        let status = Status::parse(&value.to_string()).unwrap();
        assert_eq!(status.phase().0, "validating");
        assert_eq!(status.rollback()["reason"], "candidate_pending");
        value["boot"]["deploymentId"] = "b".repeat(64).into();
        value["boot"]["entry"] = format!("mica-{}.conf", "b".repeat(64)).into();
        assert_eq!(
            Status::parse(&value.to_string()).unwrap().phase().0,
            "reboot-required"
        );
        value["state"]["candidate"] = Value::Null;
        value["state"]["failed"] = json!(["a".repeat(64)]);
        assert_eq!(
            Status::parse(&value.to_string()).unwrap().phase().0,
            "rolled-back"
        );
    }

    #[test]
    fn fit_status_distinguishes_required_fit_verification_from_uefi_secure_boot() {
        let mut value = fixture();
        value["boot"]["backend"] = json!("uboot-fit");
        value["boot"]["entry"] = json!(format!("fit:{}", "a".repeat(64)));
        value["boot"]["secureBoot"] = json!(false);
        let status = Status::parse(&value.to_string()).unwrap();
        assert_eq!(status.boot.backend, BootBackend::UbootFit);
        assert!(status.boot.boot_verified);
        assert!(!status.boot.secure_boot);
        value["boot"]["secureBoot"] = json!(true);
        assert!(Status::parse(&value.to_string()).is_err());
        value["boot"]["secureBoot"] = json!(false);
        value["boot"]["bootVerified"] = json!(false);
        assert!(Status::parse(&value.to_string()).is_err());
        value["boot"]["backend"] = json!("unknown");
        assert!(Status::parse(&value.to_string()).is_err());
    }

    #[test]
    fn invalid_or_substituted_native_status_is_refused() {
        let mut value = fixture();
        value["slots"] = json!({});
        assert!(Status::parse(&value.to_string()).is_err());
        value.as_object_mut().unwrap().remove("slots");
        value["boot"]["kernelId"] = "e".repeat(64).into();
        assert!(Status::parse(&value.to_string()).is_err());
        value = fixture();
        value["deployments"][1] = value["deployments"][0].clone();
        assert!(Status::parse(&value.to_string()).is_err());
        assert!(Status::parse(&" ".repeat(65537)).is_err());
    }
}
