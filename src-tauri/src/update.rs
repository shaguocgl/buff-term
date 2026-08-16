//! GitHub Release 版本检查。

use serde::{Deserialize, Serialize};

const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/shaguocgl/keywisp-agent-ops/releases/latest";

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub release_found: bool,
}

#[tauri::command]
pub fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
pub async fn check_for_update() -> Result<UpdateInfo, String> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let client = reqwest::Client::builder()
        .user_agent(concat!("KeyWisp-Agent-Ops/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("初始化更新检查失败: {e}"))?;
    let response = client
        .get(RELEASES_LATEST_URL)
        .send()
        .await
        .map_err(|e| format!("无法连接 GitHub 检查更新: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateInfo {
            latest_version: current_version.clone(),
            current_version,
            update_available: false,
            release_url: "https://github.com/shaguocgl/keywisp-agent-ops/releases".to_string(),
            release_found: false,
        });
    }
    if !response.status().is_success() {
        return Err(format!("GitHub 返回更新检查失败（{}）", response.status()));
    }
    let release: GithubRelease = response
        .json()
        .await
        .map_err(|e| format!("读取 GitHub 发布信息失败: {e}"))?;
    let latest_version = release.tag_name.trim_start_matches('v').to_string();

    Ok(UpdateInfo {
        update_available: is_newer(&latest_version, &current_version),
        current_version,
        latest_version,
        release_url: release.html_url,
        release_found: true,
    })
}

fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |version: &str| {
        let version = version.trim_start_matches('v');
        version
            .split_once('-')
            .map_or(version, |(stable, _)| stable)
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let candidate = parse(candidate);
    let current = parse(current);
    let len = candidate.len().max(current.len());
    (0..len)
        .find_map(|index| {
            let next = candidate.get(index).copied().unwrap_or(0);
            let installed = current.get(index).copied().unwrap_or(0);
            (next != installed).then_some(next > installed)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_semantic_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
    }

    #[test]
    fn compares_numeric_components() {
        assert!(!is_newer("1.2.0", "1.10.0"));
        assert!(is_newer("1.10.0", "1.2.0"));
    }
}
