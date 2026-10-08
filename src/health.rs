use std::net::SocketAddr;
use std::process::Command;
use std::time::Duration;

#[allow(unused_imports)]
use anyhow::{Context, Result, bail};
use tokio::time::Instant;

use crate::setup;

#[derive(Clone)]
pub struct HealthReport {
    pub codewhale_binary: Option<String>,
    pub codewhale_version: Option<String>,
    pub relay_reachable: bool,
    pub browser_ready: bool,
    pub auth_status: AuthStatus,
    pub model_settings: ModelSettings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AuthStatus {
    Unknown,
    SignedIn,
    SignedOut,
    Expired,
}

#[derive(Clone, Debug, Default)]
pub struct ModelSettings {
    pub search_enabled: bool,
    pub deep_thinking: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
}

pub async fn check_all(
    codewhale_bin: Option<&str>,
    relay_addr: Option<SocketAddr>,
    token: Option<&str>,
) -> HealthReport {
    let binary = setup::find_codewhale_binary(codewhale_bin).ok();
    let version = binary.as_ref().and_then(|bin| {
        Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });

    let relay_reachable = if let (Some(addr), Some(tok)) = (relay_addr, token) {
        check_relay(addr, tok).await
    } else {
        false
    };

    HealthReport {
        codewhale_binary: binary.map(|p| p.display().to_string()),
        codewhale_version: version,
        relay_reachable,
        browser_ready: false,
        auth_status: AuthStatus::Unknown,
        model_settings: ModelSettings::default(),
    }
}

async fn check_relay(addr: SocketAddr, token: &str) -> bool {
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/models");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(resp) = client
            .get(&url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            && resp.status().is_success()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}
