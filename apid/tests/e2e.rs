//! End-to-end test: private `dbus-daemon --session` + real `mosd` + real
//! `apid`, driven over HTTPS/HTTP with a real client.
//!
//! Everything lives in tempdirs on ephemeral ports; `MOSD_DRY_RUN=1` keeps
//! the host untouched. A missing `dbus-daemon` is a failure, never a skip.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::Context;
use reqwest::StatusCode;
use reqwest::header::{ALLOW, CONTENT_TYPE, LOCATION, SET_COOKIE};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn dbus_daemon() -> PathBuf {
    let fixed = PathBuf::from("/usr/bin/dbus-daemon");
    if fixed.exists() {
        return fixed;
    }
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("dbus-daemon"))
                .find(|candidate| candidate.exists())
        })
        .unwrap_or_else(|| {
            panic!(
                "dbus-daemon was not found at /usr/bin/dbus-daemon or on PATH; install the \
                 dbus-daemon package because this real-bus test must not skip"
            )
        })
}

fn find_mosd() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("MOSD_BIN") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().context("locate test executable")?;
    let candidate = exe
        .parent()
        .and_then(|deps| deps.parent())
        .context("test executable has no target profile directory")?
        .join("mosd");
    anyhow::ensure!(
        candidate.exists(),
        "mosd binary not found at {}; build it with `cargo build -p mosd` or set MOSD_BIN",
        candidate.display()
    );
    Ok(candidate)
}

#[zbus::proxy(
    interface = "com.mos.mosd1",
    default_service = "com.mos.mosd",
    default_path = "/com/mos/mosd"
)]
trait Mosd {
    fn get_settings(&self, path: &str) -> zbus::Result<String>;
    fn get_state(&self, path: &str) -> zbus::Result<String>;
}

fn wait_for_line(stdout: ChildStdout, prefix: &'static str) -> anyhow::Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.starts_with(prefix) {
                let _ = tx.send(line);
                break;
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(30))
        .with_context(|| format!("timed out waiting for `{prefix}` on stdout"))
}

fn http_client(cookies: bool) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(cookies)
        .build()
        .context("build reqwest client")
}

fn location(response: &reqwest::Response) -> &str {
    response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("(no Location header)")
}

fn module_script_src(html: &str) -> Option<&str> {
    let source = html.split_once("<script type=\"module\"")?.1;
    let source = source.split_once("src=\"")?.1;
    source.split_once('"').map(|(path, _)| path)
}

async fn response_json(response: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    serde_json::from_str(&response.text().await?).context("parse JSON response")
}

async fn wait_for_task(
    client: &reqwest::Client,
    https_base: &str,
    task_id: &str,
) -> anyhow::Result<serde_json::Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = client
                .get(format!("{https_base}/api/v1/tasks/{task_id}"))
                .send()
                .await?;
            anyhow::ensure!(response.status() == StatusCode::OK, "task lookup failed");
            let task = response_json(response).await?;
            if task["status"] == "finished" {
                break anyhow::Ok(task);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("apply task did not finish")?
}

#[tokio::test(flavor = "multi_thread")]
async fn web_flow_end_to_end() -> anyhow::Result<()> {
    let mut bus_child = Command::new(dbus_daemon())
        .args(["--session", "--print-address=1", "--nofork"])
        .stdout(Stdio::piped())
        .spawn()?;
    let bus_stdout = bus_child.stdout.take().expect("piped stdout");
    let _bus_guard = ChildGuard(bus_child);
    let mut address = String::new();
    BufReader::new(bus_stdout).read_line(&mut address)?;
    let address = address.trim().to_string();
    anyhow::ensure!(!address.is_empty(), "dbus-daemon printed no address");

    let dir = tempfile::tempdir()?;
    let settings_path = dir.path().join("settings.toml");
    // The `/mos/config/` namespace mosd reads its configuration from. It must
    // exist before the daemon starts: an absent namespace is the DATA medium
    // being gone, and mosd refuses to start rather than render a configuration
    // nobody chose (PLAN-070 §5.2.6).
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir)?;
    let shadow_path = dir.path().join("shadow");
    std::fs::write(&shadow_path, "root:!:20000:0:99999:7:::\n")?;
    let _mosd_guard = ChildGuard(
        Command::new(find_mosd()?)
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("MOSD_BUS", "session")
            .env("MOSD_DRY_RUN", "1")
            .env("MOSD_SETTINGS_PATH", &settings_path)
            .env("MOSD_CONFIG_DIR", &config_dir)
            .env("MOSD_SHADOW_PATH", &shadow_path)
            .spawn()?,
    );

    let mut apid_child = Command::new(env!("CARGO_BIN_EXE_apid"))
        .env("DBUS_SESSION_BUS_ADDRESS", &address)
        .env("APID_BUS", "session")
        .env("APID_STATE_DIR", dir.path().join("apid"))
        .env("APID_HTTPS_ADDR", "127.0.0.1:0")
        .env("APID_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()?;
    let apid_stdout = apid_child.stdout.take().expect("piped stdout");
    let _apid_guard = ChildGuard(apid_child);
    let marker = wait_for_line(apid_stdout, "APID_LISTENING ")?;
    let field = |name: &str| {
        marker
            .split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .map(str::to_string)
            .with_context(|| format!("`{name}=` missing from marker `{marker}`"))
    };
    let https_addr = field("https")?;
    let http_addr = field("http")?;
    let https_base = format!("https://{https_addr}");

    let connection = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let proxy = MosdProxy::new(&connection).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while proxy.get_settings("").await.is_err() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("mosd did not come up on the private bus")?;

    let admin = http_client(true)?;
    let anonymous = http_client(false)?;

    // Static routing is independent of setup and authentication.
    let response = admin.get(format!("{https_base}/")).send().await?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/_ui/");
    for path in ["/_ui", "/_ui/", "/_ui/network"] {
        let response = admin.get(format!("{https_base}{path}")).send().await?;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        assert!(
            response
                .text()
                .await?
                .contains("<title>mos console</title>")
        );
    }
    let index = admin
        .get(format!("{https_base}/_ui/"))
        .send()
        .await?
        .text()
        .await?;
    let script = module_script_src(&index).context("built-in index has no module script")?;
    anyhow::ensure!(
        script.starts_with("/_ui/assets/") && script.ends_with(".js"),
        "built-in module script is not a hashed /_ui asset: {script}"
    );
    let response = admin.get(format!("{https_base}{script}")).send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/javascript; charset=utf-8")
    );
    assert!(!response.bytes().await?.is_empty());

    let response = anonymous
        .get(format!("{https_base}/api/v1/session"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await?["state"], "setup");

    // First-run setup creates both the one-time bearer and the browser session.
    let response = admin
        .post(format!("{https_base}/api/v1/setup"))
        .header(CONTENT_TYPE, "application/json")
        .body(r#"{"password":"e2e-password","hostname":"e2e-host"}"#)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response
        .headers()
        .get(SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .context("setup did not set a session cookie")?;
    for attribute in [
        "apid_session=",
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
    ] {
        assert!(
            cookie.contains(attribute),
            "missing {attribute} in {cookie}"
        );
    }
    let setup = response_json(response).await?;
    let bearer = setup["token"].as_str().context("setup token")?.to_string();
    let csrf = setup["csrfToken"]
        .as_str()
        .context("setup CSRF token")?
        .to_string();

    let response = admin
        .get(format!("{https_base}/api/v1/session"))
        .send()
        .await?;
    let session = response_json(response).await?;
    assert_eq!(session["state"], "authenticated");
    assert_eq!(session["csrfToken"], csrf);

    // Cookie reads work, while cookie writes require the session's CSRF proof.
    let response = admin
        .get(format!("{https_base}/api/v1/health"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let health = response_json(response).await?;
    assert_eq!(health["apid"], "ok");
    assert_eq!(health["mosd"], "ok");

    let response = anonymous
        .get(format!("{https_base}/api/v1/health"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = admin
        .put(format!("{https_base}/api/v1/settings/hostname"))
        .header(CONTENT_TYPE, "application/json")
        .body(r#""must-not-apply""#)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(response).await?["error"]["code"],
        "csrf_invalid"
    );
    assert_eq!(proxy.get_settings("hostname").await?, "\"e2e-host\"");

    let response = admin
        .put(format!("{https_base}/api/v1/settings/hostname"))
        .header("x-csrf-token", &csrf)
        .header(CONTENT_TYPE, "application/json")
        .body(r#""e2e-host2""#)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = response_json(response).await?;
    let task = wait_for_task(
        &admin,
        &https_base,
        accepted["taskId"].as_str().context("taskId")?,
    )
    .await?;
    assert_eq!(task["outcome"], "succeeded");
    assert_eq!(proxy.get_settings("hostname").await?, "\"e2e-host2\"");

    // Network reads distinguish configured intent from observation. Dry-run
    // intentionally has no systemd-networkd observer, so availability is false.
    let response = admin
        .get(format!("{https_base}/api/v1/network"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let network = response_json(response).await?;
    assert!(network["configuredCount"].is_u64());
    assert_eq!(network["observed"]["available"], false);
    assert!(network["observed"]["interfaces"].is_array());

    // The setup bearer is the same API credential without a CSRF requirement.
    let response = anonymous
        .get(format!("{https_base}/api/v1/settings/hostname"))
        .bearer_auth(&bearer)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await?, "e2e-host2");

    // Every retired form mutation is inert and cannot change the device.
    for path in [
        "/containers/enable",
        "/mqtt/enable",
        "/setup",
        "/login",
        "/logout",
        "/hostname",
        "/network",
        "/ssh/enable",
        "/power/reboot",
        "/builtin/tokens",
    ] {
        let response = admin
            .post(format!("{https_base}{path}"))
            .form(&[("hostname", "legacy-must-not-apply"), ("enabled", "on")])
            .send()
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "POST {path}"
        );
    }
    assert_eq!(proxy.get_settings("hostname").await?, "\"e2e-host2\"");

    // Wrong methods inside /api retain the JSON error contract and Allow header.
    let response = admin
        .post(format!("{https_base}/api/v1/meta"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers().get(ALLOW).context("Allow")?, "GET,HEAD");
    assert_eq!(
        response_json(response).await?["error"]["code"],
        "method_not_allowed"
    );

    // Power actions are API-only and CSRF-protected. The daemon is verified
    // dry-run before dispatch, so no host power control can be constructed.
    assert_eq!(proxy.get_state("dry_run").await?, "true");
    let response = admin
        .post(format!("{https_base}/api/v1/actions/reboot"))
        .header("x-csrf-token", &csrf)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(json) = proxy.get_state("power").await
                && serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .as_ref()
                    .is_some_and(|value| value["last_action"] == "reboot")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("reboot action never reached mosd")?;

    // Logout is itself a CSRF-protected API mutation; login returns a new token.
    let response = admin
        .delete(format!("{https_base}/api/v1/session"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = admin
        .delete(format!("{https_base}/api/v1/session"))
        .header("x-csrf-token", &csrf)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = admin
        .get(format!("{https_base}/api/v1/session"))
        .send()
        .await?;
    assert_eq!(response_json(response).await?["state"], "unauthenticated");
    let response = admin
        .post(format!("{https_base}/api/v1/session"))
        .header(CONTENT_TYPE, "application/json")
        .body(r#"{"password":"e2e-password"}"#)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let logged_in = response_json(response).await?;
    assert_eq!(logged_in["state"], "authenticated");
    assert!(logged_in["csrfToken"].is_string());

    // The plain HTTP listener only redirects to HTTPS and preserves the path.
    let response = anonymous
        .get(format!("http://{http_addr}/_ui/network?from=e2e"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    let target = location(&response);
    assert!(
        target.starts_with("https://"),
        "unexpected redirect: {target}"
    );
    assert!(
        target.ends_with("/_ui/network?from=e2e"),
        "unexpected redirect: {target}"
    );

    // Keep an explicit content-type assertion on API setup's sibling route.
    let response = anonymous
        .get(format!("{https_base}/api/versions"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    );

    Ok(())
}
