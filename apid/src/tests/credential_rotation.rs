use super::*;
use serde_json::Value;

struct PausedAccessRead {
    inner: Arc<FakeSettings>,
    held: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl SettingsApi for PausedAccessRead {
    async fn get_settings(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let value = self.inner.get_settings(path).await?;
        if path == "access" && !self.held.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(value)
    }
    async fn set_settings(&self, path: &str, value: &Value) -> anyhow::Result<String> {
        self.inner.set_settings(path, value).await
    }
    async fn get_task(&self, id: &str) -> anyhow::Result<TaskRecord> {
        self.inner.get_task(id).await
    }
    async fn get_state(&self, path: &str) -> anyhow::Result<Value> {
        self.inner.get_state(path).await
    }
    async fn get_network_state(&self) -> anyhow::Result<Value> {
        self.inner.get_network_state().await
    }
    async fn get_time_status(&self) -> anyhow::Result<Value> {
        self.inner.get_time_status().await
    }
    async fn get_storage_status(&self) -> anyhow::Result<Value> {
        self.inner.get_storage_status().await
    }
    async fn get_system_info(&self) -> anyhow::Result<Value> {
        self.inner.get_system_info().await
    }
    async fn get_telemetry(&self) -> anyhow::Result<Value> {
        self.inner.get_telemetry().await
    }
    async fn get_observed_network(&self) -> anyhow::Result<Value> {
        self.inner.get_observed_network().await
    }
    async fn get_failure_evidence(&self) -> anyhow::Result<Value> {
        self.inner.get_failure_evidence().await
    }
    async fn reboot(&self) -> anyhow::Result<()> {
        self.inner.reboot().await
    }
    async fn power_off(&self) -> anyhow::Result<()> {
        self.inner.power_off().await
    }
    async fn set_transient_root_password(&self, password: &str) -> anyhow::Result<String> {
        self.inner.set_transient_root_password(password).await
    }
    async fn rotate_wireguard_key(&self, iface: &str) -> anyhow::Result<String> {
        self.inner.rotate_wireguard_key(iface).await
    }
    async fn get_update_state(&self) -> anyhow::Result<Value> {
        self.inner.get_update_state().await
    }
    async fn check_update(&self) -> anyhow::Result<()> {
        self.inner.check_update().await
    }
    async fn fetch_update(&self) -> anyhow::Result<()> {
        self.inner.fetch_update().await
    }
    async fn install_update(&self, bundle: &str) -> anyhow::Result<()> {
        self.inner.install_update(bundle).await
    }
    async fn confirm_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        self.inner.confirm_deployment(deployment_id).await
    }

    async fn reject_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        self.inner.reject_deployment(deployment_id).await
    }

    async fn rollback_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        self.inner.rollback_deployment(deployment_id).await
    }
    async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<Value> {
        self.inner.set_reboot_override(seconds).await
    }
    async fn set_update_config(&self, patch: &Value) -> anyhow::Result<Value> {
        self.inner.set_update_config(patch).await
    }
}

#[tokio::test]
async fn old_password_login_cannot_survive_a_completed_password_change() {
    let (tree, token) = with_token(configured_tree("audit-old-password"));
    let paused = Arc::new(PausedAccessRead {
        inner: Arc::new(FakeSettings::new(tree)),
        held: std::sync::atomic::AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let router = app(AppState::new(paused.clone(), SIGNING_KEY));
    let login_router = router.clone();
    let pending = tokio::spawn(async move {
        json_request(
            &login_router,
            "POST",
            "/api/v1/session",
            json!({"password": "audit-old-password"}),
            None,
            None,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), paused.entered.notified())
        .await
        .unwrap();
    let changed = send(&router, Request::builder().method("POST")
        .uri("/api/v1/actions/change-password")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"currentPassword": "audit-old-password", "newPassword": "audit-new-password"}).to_string())).unwrap()).await;
    assert_eq!(changed.status(), StatusCode::NO_CONTENT);
    paused.release.notify_one();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let new_cookie = login(&router, "audit-new-password").await;
    assert_eq!(
        get(&router, "/api/v1/settings/hostname", Some(&new_cookie))
            .await
            .status(),
        StatusCode::OK
    );
}
