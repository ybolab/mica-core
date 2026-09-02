//! The OpenAPI document describing apid's `/api` surface.
//!
//! Generated from the handlers rather than written beside them, and committed
//! as `mosd/apid/openapi.json`. `--openapi` prints it and a test asserts the
//! committed copy is exactly what this module produces, so the spec cannot
//! describe a route the code does not serve or miss one it does.

use utoipa::OpenApi;

/// The document: its identity, and every route declared under `/api`.
///
/// `version` is the API major version §2.1 puts in the path segment, not
/// apid's package version. This document describes the HTTP contract, and the
/// contract is what `/api/versions` names.
///
/// The listed handlers carry their own `utoipa::path` attributes, and the
/// schemas of the bodies they name are collected from them; there is no second
/// list here to keep in step with the first.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "apid",
        version = "v1",
        description = "The appliance management API served over HTTPS on the device."
    ),
    paths(
        crate::routes::api_versions,
        crate::routes::api_v1_session_status,
        crate::routes::api_v1_session_create,
        crate::routes::api_v1_session_delete,
        crate::routes::api_v1_ui_status,
        crate::routes::api_v1_ui_bundles,
        crate::routes::api_v1_ui_upload,
        crate::routes::api_v1_ui_activate,
        crate::routes::api_v1_ui_deactivate,
        crate::routes::api_v1_ui_delete,
        crate::routes::api_v1_meta,
        crate::routes::api_v1_health,
        crate::routes::api_v1_settings,
        crate::routes::api_v1_settings_write,
        crate::routes::api_v1_state,
        crate::routes::api_v1_time_status,
        crate::routes::api_v1_storage_status,
        crate::routes::api_v1_system_info,
        crate::routes::api_v1_system_telemetry,
        crate::routes::api_v1_network_status,
        crate::routes::api_v1_diagnostics_list,
        crate::routes::api_v1_diagnostics_collect,
        crate::routes::api_v1_diagnostics_snapshot,
        crate::routes::api_v1_diagnostics_delete,
        crate::routes::api_v1_tasks_list,
        crate::routes::api_v1_task,
        crate::routes::api_v1_change_password,
        crate::routes::api_v1_wireguard_rotate,
        crate::routes::api_v1_tokens_list,
        crate::routes::api_v1_tokens_mint,
        crate::routes::api_v1_tokens_revoke,
        crate::routes::api_v1_ssh_keys_list,
        crate::routes::api_v1_ssh_keys_add,
        crate::routes::api_v1_ssh_keys_remove,
        crate::routes::api_v1_wifi_networks_list,
        crate::routes::api_v1_wifi_networks_add,
        crate::routes::api_v1_wifi_networks_remove,
        crate::routes::api_v1_network_read,
        crate::routes::api_v1_network_write,
        crate::routes::api_v1_network_iface_write,
        crate::routes::api_v1_network_iface_remove,
        crate::routes::api_v1_peers_list,
        crate::routes::api_v1_peers_add,
        crate::routes::api_v1_peers_remove,
        crate::routes::api_v1_reboot,
        crate::routes::api_v1_poweroff,
        crate::routes::api_v1_transient_root_password,
        crate::routes::api_v1_setup,
        crate::provisioning_api::api_v1_provisioning_status,
        crate::update_api::api_v1_update_state,
        crate::update_api::api_v1_update_check,
        crate::update_api::api_v1_update_fetch,
        crate::update_api::api_v1_update_install,
        crate::update_api::api_v1_update_mark,
        crate::update_api::api_v1_update_rollback,
        crate::update_api::api_v1_update_reboot_override
    )
)]
struct ApiDoc;

/// The document as the exact bytes `--openapi` prints and `openapi.json`
/// holds: pretty-printed, with one trailing newline.
pub fn document_json() -> String {
    let mut document = ApiDoc::openapi();
    // The crates declare no `license`, so the derive fills the object in from
    // the empty `CARGO_PKG_LICENSE`. A licence object whose only required
    // field is the empty string states nothing; there is no licence to name,
    // so there is no object.
    document.info.license = None;
    let mut json = document
        .to_pretty_json()
        .expect("a document of derived schemas serialises");
    json.push('\n');
    json
}
