//! Custom UI compatibility stays independent from the reserved built-in SPA.

use std::fs;

use super::*;
use crate::bundle::Store;
use crate::startup::{self, BundleState, SERVED_API_VERSIONS};

const CUSTOM_INDEX: &str = "<!doctype html><title>custom</title>";

fn install_declaring(files: &[(&str, &str)], served: &[&str]) -> TempDir {
    let dir = TempDir::new().expect("temp bundle store");
    let store = Store::new(dir.path());
    let staging = store.staging_dir(1);
    for (relative, contents) in files {
        let path = staging.join(relative);
        fs::create_dir_all(path.parent().expect("staged parent")).expect("create staged parent");
        fs::write(path, contents).expect("write staged file");
    }
    store.activate(1, served).expect("activate staged UI");
    dir
}

#[tokio::test]
async fn a_bundle_that_intersects_the_served_set_remains_the_root_ui() {
    let manifest = r#"{"name":"demo","version":"1.0",
        "immutableDir":"assets","apiVersions":["v0","v1"]}"#;
    let bundle = install_declaring(
        &[("index.html", CUSTOM_INDEX), ("mica-ui.json", manifest)],
        SERVED_API_VERSIONS,
    );
    let store = Store::new(bundle.path());

    let state = startup::discover(
        store,
        std::sync::Arc::new(crate::audit::Audit::journal_only()),
    )
    .await;
    assert!(matches!(state, BundleState::Active { generation: 1, .. }));

    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    assert_eq!(
        body_string(get(&router, "/", None).await).await,
        CUSTOM_INDEX
    );

    let built_in = get(&router, "/_ui/", None).await;
    assert_eq!(built_in.status(), StatusCode::OK);
    assert!(
        body_string(built_in)
            .await
            .contains("<title>mica console</title>")
    );
}
