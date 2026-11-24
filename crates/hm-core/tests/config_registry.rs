//! The paired state API exposes truthful descriptive shapes without adding
//! package-writable fields or implying unsupported runtime application.
use hm_core::{
    api::{router, AppState},
    store::Store,
    tool::mocks::MockInject,
};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc};

#[tokio::test]
async fn management_state_exposes_native_field_shapes_and_preserves_apply_limits() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let dir = std::env::temp_dir().join(format!(
        "hm-registry-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    let state = AppState::with_overrides(
        store,
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let token = state.auth.provision("registry-shape-test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/api/v1/config/state"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let state = client
        .get(format!("{base}/api/v1/config/state"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let fields = state["fields"].as_array().unwrap();
    let field = |path: &str| {
        fields
            .iter()
            .find(|field| field["path"] == path)
            .unwrap()
            .clone()
    };
    assert_eq!(
        field("unit_acoustic_profiles.*.volume_cap")["kind"],
        "number"
    );
    assert_eq!(
        field("unit_acoustic_profiles.*.mic_calibrated")["kind"],
        "boolean"
    );
    assert_eq!(field("mode.followup_credits")["kind"], "integer");
    assert_eq!(field("mode.followup_credits")["nullable"], true);
    assert_eq!(field("mode.system_prompt_override")["nullable"], true);
    assert_eq!(field("mode.vad")["nullable"], true);
    assert_eq!(field("music.library")["kind"], "object");
    assert_eq!(field("music.library")["nullable"], true);
    assert_eq!(field("weather.places")["array_item_kind"], "object");
    assert_eq!(field("weather.places")["supported"], true);
    assert_eq!(field("weather.places[].latitude")["supported"], false);
    assert_eq!(field("weather.api_key_env")["scope"], "node");
    assert_eq!(
        field("openapi.operations[].verification.expected")["kind"],
        "json"
    );
    assert_eq!(
        field("unit_acoustic_profiles.*.onset_threshold_dbfs")["nullable"],
        true
    );
    assert_eq!(
        field("unit_acoustic_profiles.*.onset_threshold_dbfs")["unit"],
        "dBFS"
    );
    assert_eq!(
        field("unit_acoustic_profiles.*.onset_threshold_dbfs")["scope"],
        "unit"
    );
    assert_eq!(
        fields
            .iter()
            .filter(|field| field["supported"] == true)
            .count(),
        123
    );
    for field in fields.iter().filter(|field| field["supported"] == false) {
        assert_eq!(field["purposes"], serde_json::json!([]));
        assert_eq!(field["apply"], "unsupported_in_slice");
    }
    if let Ok(directory) = std::env::var("HM_TEST_CONFIG_EVIDENCE_DIR") {
        let path = std::path::Path::new(&directory);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("api-field-catalog.json"),
            serde_json::to_vec_pretty(fields).unwrap(),
        )
        .unwrap();
    }
    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}
