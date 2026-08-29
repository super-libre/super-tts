// SPDX-License-Identifier: GPL-3.0-only
use super::*;
use serde_json::{Value, json};

fn make_request(command: &str, data: Option<Value>) -> DaemonRequest {
    DaemonRequest {
        command: command.to_string(),
        client_id: None,
        event_types: None,
        client_info: None,
        since_timestamp: None,
        limit: None,
        event_type: None,
        data,
        language: None,
        enabled: None,
    }
}

#[test]
fn set_allow_online_models_parses() {
    let mut request = make_request("set_allow_online_models", None);
    request.enabled = Some(true);
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetAllowOnlineModels { enabled } => assert!(enabled),
        _ => panic!("expected Command::SetAllowOnlineModels"),
    }
}

#[test]
fn set_allow_online_models_missing_enabled_fails() {
    let request = make_request("set_allow_online_models", None);
    let result = Command::try_from(request);
    assert!(result.is_err());
}

#[test]
fn get_allow_online_models_parses() {
    let request = make_request("get_allow_online_models", None);
    let command = Command::try_from(request).expect("command should parse");
    assert!(matches!(command, Command::GetAllowOnlineModels));
}

#[test]
fn response_with_allow_online_models() {
    let response = DaemonResponse::success().with_allow_online_models(true);
    assert_eq!(response.allow_online_models, Some(true));

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["allow_online_models"], true);
}

#[test]
fn set_allow_online_models_false() {
    let mut request = make_request("set_allow_online_models", None);
    request.enabled = Some(false);
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetAllowOnlineModels { enabled } => assert!(!enabled),
        _ => panic!("expected Command::SetAllowOnlineModels"),
    }
}

#[test]
fn response_allow_online_models_false_serializes() {
    let response = DaemonResponse::success().with_allow_online_models(false);
    assert_eq!(response.allow_online_models, Some(false));

    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["allow_online_models"], false);
}

#[test]
fn response_allow_online_models_skipped_when_none() {
    let response = DaemonResponse::success();
    assert_eq!(response.allow_online_models, None);

    let json = serde_json::to_value(&response).unwrap();
    assert!(json.get("allow_online_models").is_none());
}

#[test]
fn set_model_parses_online_models() {
    let cases: &[&str] = &[
        "kokoro-1",
        "gpt-4o-mini-tts",
        "tts-1-hd",
        "piper-mini-latest",
        "nova-3",
    ];
    for model_name in cases {
        let request = make_request("set_model", Some(json!({ "model": model_name})));
        let command = Command::try_from(request)
            .unwrap_or_else(|e| panic!("set_model should parse {model_name}: {e}"));
        match command {
            Command::SetModel { model, source } => {
                assert_eq!(model.clone(), *model_name);
                // No source supplied → empty; the daemon resolves it against
                // the active backend (see endpoints/v1/active_model.md).
                assert_eq!(source, "");
            }
            _ => panic!("expected Command::SetModel for {model_name}"),
        }
    }
}

#[test]
fn set_model_parses_local_name() {
    let request = make_request(
        "set_model",
        Some(json!({ "model": "kokoro-tiny", "provider": "local_kokoro" })),
    );
    let command = Command::try_from(request).expect("should parse");
    match command {
        Command::SetModel { model, source } => {
            assert_eq!(model, "kokoro-tiny");
            assert_eq!(source, "");
        }
        _ => panic!("expected Command::SetModel"),
    }
}

#[test]
fn set_model_passes_source_repo_through() {
    let request = make_request(
        "set_model",
        Some(json!({
            "model": "piper-mini",
            "provider": "local_piper",
            "source": "github.com/super-tts/piper",
        })),
    );
    let command = Command::try_from(request).expect("should parse");
    match command {
        Command::SetModel { source, .. } => {
            assert_eq!(source, "github.com/super-tts/piper");
        }
        _ => panic!("expected Command::SetModel"),
    }
}

#[test]
fn set_custom_models_dir_parses_with_path() {
    let request = make_request(
        "set_custom_models_dir",
        Some(json!({ "path": "/tmp/models" })),
    );
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetCustomModelsDir { path } => {
            assert_eq!(path.as_deref(), Some("/tmp/models"));
        }
        _ => panic!("expected Command::SetCustomModelsDir"),
    }
}

#[test]
fn set_custom_models_dir_parses_with_null() {
    let request = make_request("set_custom_models_dir", Some(json!({ "path": null })));
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetCustomModelsDir { path } => {
            assert!(path.is_none());
        }
        _ => panic!("expected Command::SetCustomModelsDir"),
    }
}

#[test]
fn set_custom_models_dir_parses_without_data() {
    let request = make_request("set_custom_models_dir", None);
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetCustomModelsDir { path } => {
            assert!(path.is_none());
        }
        _ => panic!("expected Command::SetCustomModelsDir"),
    }
}

#[test]
fn list_backends_parses() {
    let request = make_request("list_backends", None);
    let command = Command::try_from(request).expect("command should parse");
    assert!(matches!(command, Command::ListBackends));
}

#[test]
fn unload_active_model_parses() {
    let request = make_request("unload_active_model", None);
    let command = Command::try_from(request).expect("command should parse");
    assert!(matches!(command, Command::UnloadActiveModel));
}

#[test]
fn set_backend_option_parses() {
    let request = make_request(
        "set_backend_option",
        Some(json!({
            "source": "github.com/super-tts/openai",
            "name": "base_url",
            "value": "https://gw.example",
        })),
    );
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetBackendOption {
            source,
            name,
            value,
        } => {
            assert_eq!(source, "github.com/super-tts/openai");
            assert_eq!(name, "base_url");
            assert_eq!(value, "https://gw.example");
        }
        _ => panic!("expected Command::SetBackendOption"),
    }
}

#[test]
fn set_backend_option_absent_value_clears() {
    let request = make_request(
        "set_backend_option",
        Some(json!({ "source": "s", "name": "base_url" })),
    );
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetBackendOption { value, .. } => assert_eq!(value, ""),
        _ => panic!("expected Command::SetBackendOption"),
    }
}

#[test]
fn set_backend_option_missing_source_fails() {
    let request = make_request("set_backend_option", Some(json!({ "name": "base_url" })));
    assert!(Command::try_from(request).is_err());
}

#[test]
fn response_with_backends_serializes() {
    let response =
        DaemonResponse::success().with_backends(json!([{ "source": "x", "models": [] }]));
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["backends"][0]["source"], "x");
}

#[test]
fn set_active_backend_parses() {
    let request = make_request(
        "set_active_backend",
        Some(json!({ "source": "github.com/super-tts/openai" })),
    );
    let command = Command::try_from(request).expect("command should parse");
    match command {
        Command::SetActiveBackend { source } => {
            assert_eq!(source, "github.com/super-tts/openai");
        }
        _ => panic!("expected Command::SetActiveBackend"),
    }
}

#[test]
fn set_active_backend_missing_source_fails() {
    let request = make_request("set_active_backend", Some(json!({})));
    assert!(
        Command::try_from(request).is_err(),
        "set_active_backend without source must be rejected"
    );
}

#[test]
fn set_active_backend_without_data_fails() {
    let request = make_request("set_active_backend", None);
    assert!(Command::try_from(request).is_err());
}

#[test]
fn get_active_backend_parses() {
    let request = make_request("get_active_backend", None);
    let command = Command::try_from(request).expect("command should parse");
    assert!(matches!(command, Command::GetActiveBackend));
}

#[test]
fn clear_active_backend_parses() {
    let request = make_request("clear_active_backend", None);
    let command = Command::try_from(request).expect("command should parse");
    assert!(matches!(command, Command::ClearActiveBackend));
}

#[test]
fn response_with_active_backend_payload_serializes() {
    let payload = json!({
        "source": "github.com/super-tts/openai",
        "name": "OpenAI",
        "model_loaded": false,
    });
    let response = DaemonResponse::success().with_active_backend(payload);
    assert!(response.active_backend.is_some());

    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(
        serialized["active_backend"]["source"],
        "github.com/super-tts/openai"
    );
    assert_eq!(serialized["active_backend"]["model_loaded"], false);
}

/// `clear_active_backend` returns `active_backend: null` on the wire, which
/// the response carries as `Some(Value::Null)` (distinct from "field
/// absent"). The serde skip-if-None means `null` *is* serialized — only an
/// unset field is omitted.
#[test]
fn response_active_backend_null_round_trips() {
    let response = DaemonResponse::success().with_active_backend(Value::Null);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["active_backend"], Value::Null);
}

#[test]
fn response_active_backend_absent_is_skipped() {
    let response = DaemonResponse::success();
    assert!(response.active_backend.is_none());

    let serialized = serde_json::to_value(&response).unwrap();
    assert!(
        serialized.get("active_backend").is_none(),
        "skip_serializing_if=Option::is_none should omit the field"
    );
}

/// `resolved_accel` is doubly `Option` so a response that never mentions
/// devices at all omits the key, while `/active_device` can still emit a
/// literal `null` for "gpu requested, nothing has resolved yet" rather than
/// dropping the key like every other irrelevant field on this kitchen-sink
/// response type.
#[test]
fn resolved_accel_is_omitted_unless_set_then_may_serialize_as_null() {
    let untouched = serde_json::to_value(DaemonResponse::success()).expect("serialize");
    assert!(
        untouched.get("resolved_accel").is_none(),
        "a response that never calls with_resolved_accel must not mention the key: {untouched}"
    );

    let unresolved =
        serde_json::to_value(DaemonResponse::success().with_resolved_accel(None)).expect("ser");
    assert_eq!(unresolved["resolved_accel"], Value::Null);

    let resolved = serde_json::to_value(
        DaemonResponse::success().with_resolved_accel(Some("cuda".to_string())),
    )
    .expect("ser");
    assert_eq!(resolved["resolved_accel"], "cuda");
}

/// The `host.{cuda,rocm,vulkan}` field names are published in
/// `docs/protocol/endpoints/v1/gpu_info.md` and other work builds against
/// them; pin the exact wire shape here rather than trusting the derive.
#[test]
fn gpu_host_info_serializes_the_published_field_names() {
    let host = GpuHostInfo {
        cuda: Some(CudaHostInfo {
            driver_version: "13.3".to_string(),
        }),
        rocm: None,
        vulkan: Some(VulkanHostInfo {
            api_version: "1.4.354".to_string(),
        }),
    };
    let value = serde_json::to_value(
        DaemonResponse::success()
            .with_gpu_info(vec![GpuInfo {
                name: "NVIDIA GeForce RTX 3090".to_string(),
                vendor: "nvidia".to_string(),
                total_bytes: 25_757_220_864,
                free_bytes: None,
                used_bytes: None,
                arch_target: Some("sm_86".to_string()),
            }])
            .with_gpu_host_info(host),
    )
    .expect("serialize");
    assert_eq!(value["host"]["cuda"]["driver_version"], "13.3");
    assert_eq!(value["host"]["rocm"], Value::Null);
    assert_eq!(value["host"]["vulkan"]["api_version"], "1.4.354");
    assert_eq!(value["gpu_info"][0]["arch_target"], "sm_86");
}
