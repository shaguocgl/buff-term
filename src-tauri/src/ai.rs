use crate::credentials;
use crate::db::Db;
use crate::models::{AiModel, AiProvider, AiRule};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Deserialize)]
pub struct AiModelInput {
    pub label: String,
    pub model: String,
    #[serde(default)]
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
pub struct AiProviderInput {
    pub name: String,
    pub base_url: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<AiModelInput>,
    #[serde(default)]
    pub api_key: Option<String>,
}

fn default_protocol() -> String {
    "openai-compatible".to_string()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn resolve_active_ai(db: &Db) -> Result<(AiProvider, String), String> {
    let providers = db
        .list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))?;
    let provider = providers
        .into_iter()
        .find(|p| p.enabled)
        .ok_or_else(|| "未配置启用的 AI 平台，请先在左侧 AI 配置中添加".to_string())?;
    let model = provider
        .models
        .iter()
        .find(|m| m.is_active)
        .or_else(|| provider.models.first())
        .map(|m| m.model.clone())
        .ok_or_else(|| "该平台未配置模型，请到 AI 配置中添加".to_string())?;
    Ok((provider, model))
}

#[tauri::command]
pub fn list_ai_providers(db: State<'_, Db>) -> Result<Vec<AiProvider>, String> {
    db.list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))
}

#[tauri::command]
pub fn save_ai_provider(
    db: State<'_, Db>,
    input: AiProviderInput,
    id: Option<String>,
) -> Result<AiProvider, String> {
    let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let exists = db
        .list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))?
        .iter()
        .any(|p| p.id == id);

    if input.enabled {
        db.disable_all_ai_providers()
            .map_err(|e| format!("更新 AI 配置失败: {e}"))?;
    }
    if let Some(key) = &input.api_key {
        if !key.trim().is_empty() {
            credentials::save_api_key(&id, key)?;
        }
    }

    let provider = AiProvider {
        id: id.clone(),
        name: input.name,
        base_url: input.base_url,
        protocol: input.protocol,
        enabled: input.enabled,
        created_at: now(),
        models: Vec::new(),
    };
    if exists {
        db.update_ai_provider(&provider)
            .map_err(|e| format!("更新 AI 配置失败: {e}"))?;
    } else {
        db.insert_ai_provider(&provider)
            .map_err(|e| format!("保存 AI 配置失败: {e}"))?;
    }

    // 模型列表：至少保留一个有效模型，默认激活第一个
    let models: Vec<AiModel> = input
        .models
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let is_active = m.is_active || (idx == 0 && input.models.iter().all(|x| !x.is_active));
            AiModel {
                id: uuid::Uuid::new_v4().to_string(),
                label: m.label.clone(),
                model: m.model.clone(),
                is_active,
            }
        })
        .collect();
    db.replace_ai_models(&id, &models)
        .map_err(|e| format!("保存模型列表失败: {e}"))?;

    db.list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| "保存成功但读取失败".to_string())
}

#[tauri::command]
pub fn delete_ai_provider(db: State<'_, Db>, id: String) -> Result<(), String> {
    db.delete_ai_provider(&id)
        .map_err(|e| format!("删除 AI 配置失败: {e}"))?;
    credentials::delete_api_key(&id);
    Ok(())
}

#[tauri::command]
pub fn set_active_ai_model(
    db: State<'_, Db>,
    provider_id: String,
    model_id: String,
) -> Result<(), String> {
    db.set_active_ai_model(&provider_id, &model_id)
        .map_err(|e| format!("切换模型失败: {e}"))
}

#[tauri::command]
pub fn set_active_ai_provider(db: State<'_, Db>, provider_id: String) -> Result<(), String> {
    db.disable_all_ai_providers()
        .map_err(|e| format!("切换平台失败: {e}"))?;
    db.set_ai_provider_enabled(&provider_id, true)
        .map_err(|e| format!("切换平台失败: {e}"))
}

#[tauri::command]
pub fn list_ai_rules(db: State<'_, Db>) -> Result<Vec<AiRule>, String> {
    db.list_ai_rules()
        .map_err(|e| format!("读取智能审核规则失败: {e}"))
}

#[tauri::command]
pub fn add_ai_rule(db: State<'_, Db>, pattern: String) -> Result<AiRule, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("规则不能为空".to_string());
    }
    let rule = AiRule {
        id: uuid::Uuid::new_v4().to_string(),
        pattern: pattern.to_string(),
        enabled: true,
        created_at: now(),
    };
    db.insert_ai_rule(&rule)
        .map_err(|e| format!("保存规则失败: {e}"))?;
    Ok(rule)
}

#[tauri::command]
pub fn delete_ai_rule(db: State<'_, Db>, id: String) -> Result<(), String> {
    db.delete_ai_rule(&id)
        .map_err(|e| format!("删除规则失败: {e}"))
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

#[tauri::command]
pub async fn test_ai_provider(
    base_url: String,
    model: String,
    api_key: Option<String>,
    id: Option<String>,
) -> Result<TestResult, String> {
    let key = match api_key {
        Some(k) if !k.trim().is_empty() => k,
        _ => id
            .as_deref()
            .and_then(credentials::get_api_key)
            .unwrap_or_default(),
    };
    if key.is_empty() {
        return Ok(TestResult {
            ok: false,
            message: "未提供 API Key（Ollama 本地模型可随意填写）".to_string(),
        });
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
        "stream": false,
    });
    let resp = client
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        Ok(TestResult {
            ok: true,
            message: format!("连接成功（HTTP {}）", status.as_u16()),
        })
    } else {
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(String::from))
            .unwrap_or_else(|| text.chars().take(180).collect::<String>());
        Ok(TestResult {
            ok: false,
            message: format!("HTTP {}: {}", status.as_u16(), detail),
        })
    }
}
