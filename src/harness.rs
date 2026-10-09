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
        if name.contains("opencode") {
            HarnessKind::Opencode
        } else {
            // Unknown names fall back to the Codewhale contract; a future
            // openclaw/hermes adapter adds its own detection here.
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
