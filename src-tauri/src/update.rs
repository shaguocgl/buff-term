//! GitHub Release 版本检查，以及应用启动计数 / Star 引导状态。

use crate::db::Db;
use serde::{Deserialize, Serialize};
use tauri::State;

const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/shaguocgl/buff-term/releases/latest";

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
        .user_agent(concat!("buffTerm/", env!("CARGO_PKG_VERSION")))
        // GitHub 不可达时快速失败，避免 Tauri command 长时间挂起
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(10))
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
            release_url: "https://github.com/shaguocgl/buff-term/releases".to_string(),
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

/// 首次弹出 Star 引导的启动次数（打开超过 5 次，即第 6 次启动）。
const STAR_PROMPT_FIRST_LAUNCH: u64 = 6;
/// 「以后再说」之后再次提示所需间隔的启动次数。
const STAR_PROMPT_SNOOZE_LAUNCHES: u64 = 10;

#[derive(Debug, Serialize)]
pub struct LaunchState {
    /// 本机累计启动次数（含本次）
    pub launches: u64,
    /// 本次启动是否应展示 GitHub Star 引导弹窗
    pub show_star_prompt: bool,
}

/// 应用启动时调用一次：累计启动次数，并按规则判断是否弹出 Star 引导。
/// 规则：未永久关闭时第 6 次启动首弹；「以后再说」后每过 10 次启动再弹。
#[tauri::command]
pub fn record_launch(
    db: State<'_, std::sync::Arc<Db>>,
) -> Result<LaunchState, String> {
    let read = |key: &str| -> Result<u64, String> {
        Ok(db
            .get_setting(key)
            .map_err(|e| format!("读取启动状态失败: {e}"))?
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0))
    };
    let launches = read("launch_count")?.saturating_add(1);
    db.set_setting("launch_count", &launches.to_string())
        .map_err(|e| format!("记录启动次数失败: {e}"))?;
    let never = db
        .get_setting("star_prompt_never")
        .map_err(|e| format!("读取 Star 引导状态失败: {e}"))?
        .as_deref()
        == Some("1");
    let next = read("star_prompt_next_launch")?.max(STAR_PROMPT_FIRST_LAUNCH);
    Ok(LaunchState {
        launches,
        show_star_prompt: !never && launches >= next,
    })
}

/// 「以后再说」：把下次允许提示的启动次数向后推迟 SNOOZE_LAUNCHES 次。
#[tauri::command]
pub fn star_prompt_snooze(db: State<'_, std::sync::Arc<Db>>) -> Result<(), String> {
    let launches = db
        .get_setting("launch_count")
        .map_err(|e| format!("读取启动次数失败: {e}"))?
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    db.set_setting(
        "star_prompt_next_launch",
        &launches.saturating_add(STAR_PROMPT_SNOOZE_LAUNCHES).to_string(),
    )
    .map_err(|e| format!("保存 Star 引导状态失败: {e}"))?;
    Ok(())
}

/// 「去 Star」/「不再提示」：永久关闭引导弹窗。
#[tauri::command]
pub fn star_prompt_dismiss(db: State<'_, std::sync::Arc<Db>>) -> Result<(), String> {
    db.set_setting("star_prompt_never", "1")
        .map_err(|e| format!("保存 Star 引导状态失败: {e}"))?;
    Ok(())
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
