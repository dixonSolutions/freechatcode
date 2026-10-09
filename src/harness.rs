//! Harness adapters: turn a running relay into a spawned agent process.
//!
//! The wrapper's job is only to point a harness at the loopback relay; the
//! harness owns the turn loop, tools, permissions, and workspace (see
//! `docs/design.md`). Each harness has a different spawn contract, so it gets
//! its own adapter here rather than a shared, hardcoded argv — which is exactly
//! what made `freechatcode launch opencode` exit 1 (the launcher passed the
//! Codewhale flags to a harness that has none of them).

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// The agent harness a run launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum HarnessKind {
    /// Codewhale — spoken to with `--provider/--model/--base-url/--api-key`.
    #[default]
    Codewhale,
    /// opencode (sst/opencode) — configured via `OPENCODE_CONFIG` + `-m`.
    Opencode,
    /// Official OpenClaw — isolated agent exec with a temporary provider config.
    Openclaw,
}

impl HarnessKind {
    /// Guess the harness from the resolved binary's file name, so
    /// `freechatcode launch opencode` picks the right adapter without a flag.
    pub fn detect(binary: &Path) -> HarnessKind {
        let name = binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.contains("openclaw") {
            HarnessKind::Openclaw
        } else if name.contains("opencode") {
            HarnessKind::Opencode
        } else {
            // Explicit --harness is available for renamed executables.
            HarnessKind::Codewhale
        }
    }
}

/// Everything needed to spawn a harness, once the relay is up.
pub struct HarnessSpawn {
    /// Arguments passed to the harness binary (before the user's `--` args).
    pub argv: Vec<OsString>,
    /// Extra environment variables for the harness process.
    pub envs: Vec<(OsString, OsString)>,
    /// A temp config file that must outlive the harness process. Held here so
    /// it is removed when the run ends (auto-delete on drop).
    _config: Option<tempfile::NamedTempFile>,
    _state: Option<tempfile::TempDir>,
}

/// Build the spawn contract that points `kind` at the relay serving `model_id`.
///
/// `base_url` is the loopback relay's `/v1` base and `token` its bearer token.
pub fn spawn_contract(
    kind: HarnessKind,
    base_url: &str,
    token: &str,
    model_id: &str,
) -> Result<HarnessSpawn> {
    spawn_contract_with_models(kind, base_url, token, model_id, &[model_id.to_owned()])
}

pub fn spawn_contract_with_models(
    kind: HarnessKind,
    base_url: &str,
    token: &str,
    model_id: &str,
    models: &[String],
) -> Result<HarnessSpawn> {
    match kind {
        HarnessKind::Codewhale => Ok(HarnessSpawn {
            argv: vec![
                "--provider".into(),
                "openai".into(),
                "--model".into(),
                model_id.into(),
                "--base-url".into(),
                base_url.into(),
                "--api-key".into(),
                token.into(),
            ],
            envs: Vec::new(),
            _config: None,
            _state: None,
        }),
        HarnessKind::Opencode => {
            // opencode reads providers from a JSON config, not from flags. A
            // temp file plus the documented OPENCODE_CONFIG env var points it
            // at the relay without touching the user's own config.
            let config = serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "enabled_providers": ["freechat"],
                "provider": {
                    "freechat": {
                        "npm": "@ai-sdk/openai-compatible",
                        "name": "FreeChatCode relay",
                        "options": {
                            "baseURL": base_url,
                            "apiKey": token,
                        },
                        "models": models.iter().map(|id|(id.clone(),serde_json::json!({"name":id,"tool_call":true}))).collect::<serde_json::Map<String,serde_json::Value>>(),
                    },
                },
            });
            let mut file = tempfile::NamedTempFile::new().context("create temp opencode config")?;
            file.write_all(config.to_string().as_bytes())
                .context("write temp opencode config")?;
            let path = file.path().to_owned();
            Ok(HarnessSpawn {
                argv: vec!["-m".into(), format!("freechat/{model_id}").into()],
                envs: vec![("OPENCODE_CONFIG".into(), path.into_os_string())],
                _config: Some(file),
                _state: None,
            })
        }
        HarnessKind::Openclaw => {
            let workspace = std::env::current_dir().context("resolve OpenClaw workspace")?;
            let state = tempfile::tempdir().context("create isolated OpenClaw runtime state")?;
            let config = serde_json::json!({
                "models": {"mode":"replace", "providers": {"freechat": {
                    "baseUrl": base_url, "apiKey": token, "api":"openai-completions",
                    "models":models.iter().map(|id|serde_json::json!({
                        "id":id,"name":id,"reasoning":false,"input":["text","image"],
                        "contextWindow":131072,"maxTokens":8192,
                        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
                    })).collect::<Vec<_>>()
                }}},
                "agents":{"defaults":{"workspace":workspace,"model":{"primary":format!("freechat/{model_id}")}}}
            });
            let mut file = tempfile::NamedTempFile::new().context("create temp OpenClaw config")?;
            file.write_all(config.to_string().as_bytes())
                .context("write OpenClaw config")?;
            Ok(HarnessSpawn {
                argv: vec![
                    "agent".into(),
                    "exec".into(),
                    "--config".into(),
                    file.path().as_os_str().to_owned(),
                    "--model".into(),
                    format!("freechat/{model_id}").into(),
                    "--cwd".into(),
                    workspace.into_os_string(),
                ],
                envs: vec![(
                    "OPENCLAW_STATE_DIR".into(),
                    state.path().as_os_str().to_owned(),
                )],
                _config: Some(file),
                _state: Some(state),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_reads_the_binary_name() {
        assert_eq!(
            HarnessKind::detect(Path::new("/x/openclaw")),
            HarnessKind::Openclaw
        );
        assert_eq!(
            HarnessKind::detect(Path::new("/x/opencode")),
            HarnessKind::Opencode
        );
        assert_eq!(
            HarnessKind::detect(Path::new("/x/.opencode/bin/opencode")),
            HarnessKind::Opencode
        );
        assert_eq!(
            HarnessKind::detect(Path::new("/x/codewhale")),
            HarnessKind::Codewhale
        );
        assert_eq!(
            HarnessKind::detect(Path::new("/x/unknown-thing")),
            HarnessKind::Codewhale
        );
    }

    #[test]
    fn codewhale_contract_carries_the_relay_credentials() {
        let spawn = spawn_contract(
            HarnessKind::Codewhale,
            "http://127.0.0.1:1/v1",
            "tok",
            "deepseek-chat",
        )
        .expect("contract");
        let argv: Vec<&str> = spawn.argv.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            argv,
            vec![
                "--provider",
                "openai",
                "--model",
                "deepseek-chat",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--api-key",
                "tok",
            ]
        );
        assert!(spawn.envs.is_empty());
    }

    #[test]
    fn openclaw_contract_is_isolated_and_removed_after_spawn() {
        let spawn = spawn_contract_with_models(
            HarnessKind::Openclaw,
            "http://127.0.0.1:1/v1",
            "tok",
            "gemini",
            &["gemini".into(), "deepseek-chat".into()],
        )
        .unwrap();
        let path = std::path::PathBuf::from(&spawn.argv[3]);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            &spawn.argv[..3],
            &[
                OsString::from("agent"),
                OsString::from("exec"),
                OsString::from("--config")
            ]
        );
        assert_eq!(value["models"]["mode"], "replace");
        assert_eq!(
            value["models"]["providers"]["freechat"]["baseUrl"],
            "http://127.0.0.1:1/v1"
        );
        assert_eq!(value["models"]["providers"]["freechat"]["apiKey"], "tok");
        assert_eq!(
            value["models"]["providers"]["freechat"]["models"][1]["id"],
            "deepseek-chat"
        );
        assert_eq!(
            value["agents"]["defaults"]["model"]["primary"],
            "freechat/gemini"
        );
        assert_eq!(spawn.envs[0].0, "OPENCLAW_STATE_DIR");
        let state = std::path::PathBuf::from(&spawn.envs[0].1);
        assert!(state.exists());
        drop(spawn);
        assert!(!state.exists());
        assert!(!path.exists());
    }

    #[test]
    fn opencode_contract_selects_the_model_and_points_at_the_relay() {
        let spawn = spawn_contract(
            HarnessKind::Opencode,
            "http://127.0.0.1:1/v1",
            "tok",
            "deepseek-chat",
        )
        .expect("contract");
        let argv: Vec<&str> = spawn.argv.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(argv, vec!["-m", "freechat/deepseek-chat"]);
        assert_eq!(spawn.envs.len(), 1);
        assert_eq!(spawn.envs[0].0, "OPENCODE_CONFIG");

        // The temp config exists, is JSON, and points at the relay with the token.
        let path = std::path::PathBuf::from(&spawn.envs[0].1);
        let text = std::fs::read_to_string(&path).expect("read temp config");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let provider = &value["provider"]["freechat"];
        assert_eq!(provider["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:1/v1");
        assert_eq!(provider["options"]["apiKey"], "tok");
        assert_eq!(provider["models"]["deepseek-chat"]["tool_call"], true);
    }
}
