use keyring::Entry;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// 内存凭据缓存：钥匙串读取在每次启动中每个条目只发生一次，
/// 避免 macOS 对未签名开发版二进制反复弹出钥匙串授权提示。
fn cache() -> &'static Mutex<HashMap<(String, String), String>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(kind: &str, id: &str) -> Option<String> {
    cache()
        .lock()
        .unwrap()
        .get(&(kind.to_string(), id.to_string()))
        .cloned()
}

fn cache_set(kind: &str, id: &str, value: String) {
    cache()
        .lock()
        .unwrap()
        .insert((kind.to_string(), id.to_string()), value);
}

fn cache_remove(kind: &str, id: &str) {
    cache()
        .lock()
        .unwrap()
        .remove(&(kind.to_string(), id.to_string()));
}

fn entry(host_id: &str, kind: &str) -> Result<Entry, String> {
    Entry::new("com.keywisp.agent", &format!("{kind}:{host_id}"))
        .map_err(|e| format!("初始化钥匙串失败: {e}"))
}

pub fn save_password(host_id: &str, password: &str) -> Result<(), String> {
    if password.is_empty() {
        cache_remove("password", host_id);
        let _ = entry(host_id, "password")?.delete_password();
        return Ok(());
    }
    entry(host_id, "password")?
        .set_password(password)
        .map_err(|e| format!("保存凭据到系统钥匙串失败: {e}"))?;
    cache_set("password", host_id, password.to_string());
    Ok(())
}

pub fn get_password(host_id: &str) -> Option<String> {
    if let Some(cached) = cache_get("password", host_id) {
        return Some(cached);
    }
    let password = entry(host_id, "password").ok()?.get_password().ok()?;
    cache_set("password", host_id, password.clone());
    Some(password)
}

pub fn delete_password(host_id: &str) {
    cache_remove("password", host_id);
    if let Ok(entry) = entry(host_id, "password") {
        let _ = entry.delete_password();
    }
}

pub fn save_api_key(provider_id: &str, key: &str) -> Result<(), String> {
    if key.is_empty() {
        cache_remove("apikey", provider_id);
        delete_api_key(provider_id);
        return Ok(());
    }
    entry(provider_id, "apikey")?
        .set_password(key)
        .map_err(|e| format!("保存 API Key 到系统钥匙串失败: {e}"))?;
    cache_set("apikey", provider_id, key.to_string());
    Ok(())
}

pub fn get_api_key(provider_id: &str) -> Option<String> {
    if let Some(cached) = cache_get("apikey", provider_id) {
        return Some(cached);
    }
    let key = entry(provider_id, "apikey").ok()?.get_password().ok()?;
    cache_set("apikey", provider_id, key.clone());
    Some(key)
}

pub fn delete_api_key(provider_id: &str) {
    cache_remove("apikey", provider_id);
    if let Ok(entry) = entry(provider_id, "apikey") {
        let _ = entry.delete_password();
    }
}
