use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use keyring::Entry;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// 主密钥在系统钥匙串中的条目：SQLite 中只存密文，解密需本机钥匙串里的密钥。
const MASTER_KEY_SERVICE: &str = "com.buffterm.agent";
const MASTER_KEY_ACCOUNT: &str = "master-key";

/// 主密钥内存缓存（全局维度，一把密钥加密所有凭据）：
/// 首次从钥匙串读取/生成一次，之后解密任何凭据（AI 调用、任意主机连接）
/// 都直接走内存，不再重复访问钥匙串。
static MASTER_KEY_CACHE: OnceLock<[u8; 32]> = OnceLock::new();
static MASTER_KEY_LOCK: Mutex<()> = Mutex::new(());

/// 内存凭据缓存（明文）：解密结果只计算一次，避免反复解密与弹钥匙串授权。
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

fn key_to_hex(k: &[u8]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_to_key(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err("主密钥长度异常".to_string());
    }
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| "主密钥格式异常".to_string())?;
    }
    Ok(key)
}

/// 获取（或首次生成并保存）AES-256 主密钥。
fn master_key() -> Result<[u8; 32], String> {
    // 已缓存 → 直接返回，后续不再访问钥匙串
    if let Some(key) = MASTER_KEY_CACHE.get() {
        return Ok(*key);
    }
    let _guard = MASTER_KEY_LOCK.lock().unwrap();
    if let Some(key) = MASTER_KEY_CACHE.get() {
        return Ok(*key);
    }
    let key = load_or_create_master_key()?;
    let _ = MASTER_KEY_CACHE.set(key);
    Ok(key)
}

/// 从钥匙串读取或首次生成主密钥（仅在未缓存时调用一次）。
fn load_or_create_master_key() -> Result<[u8; 32], String> {
    let entry = Entry::new(MASTER_KEY_SERVICE, MASTER_KEY_ACCOUNT)
        .map_err(|e| format!("初始化钥匙串失败: {e}"))?;
    // 已存在 → 直接读取
    if let Ok(existing) = entry.get_password() {
        if let Ok(key) = hex_to_key(&existing) {
            return Ok(key);
        }
    }
    // 不存在 → 生成并创建；macOS Keychain 的 set 在条目已存在时会报错，
    // 若并发/重复创建冲突，则读回已存在的密钥，保证所有调用方使用同一把密钥。
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| format!("生成主密钥失败: {e}"))?;
    let hex = key_to_hex(&key);
    if entry.set_password(&hex).is_err() {
        let existing = entry
            .get_password()
            .map_err(|e| format!("保存主密钥到系统钥匙串失败: {e}"))?;
        return hex_to_key(&existing);
    }
    Ok(key)
}

/// AES-256-GCM 加密：格式 = nonce(12) || ciphertext(含 16B tag)，base64 编码。
fn encrypt_secret(plain: &str) -> Result<String, String> {
    let key = master_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| format!("生成随机 nonce 失败: {e}"))?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_bytes())
        .map_err(|e| format!("加密失败: {e}"))?;
    let mut blob = Vec::with_capacity(12 + ct.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    Ok(B64.encode(blob))
}

fn decrypt_secret(enc: &str) -> Result<String, String> {
    let key = master_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let blob = B64
        .decode(enc)
        .map_err(|e| format!("凭据密文解码失败: {e}"))?;
    if blob.len() < 12 + 16 {
        return Err("凭据密文长度异常".to_string());
    }
    let (nonce, ct) = blob.split_at(12);
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "凭据解密失败（主密钥不匹配？）".to_string())?;
    String::from_utf8(plain).map_err(|_| "凭据解密结果非法".to_string())
}

fn db() -> Option<&'static std::sync::Arc<crate::db::Db>> {
    crate::db::global()
}

// ---------- 主机密码 ----------

pub fn save_password(host_id: &str, password: &str) -> Result<(), String> {
    let db = db().ok_or_else(|| "数据库未就绪".to_string())?;
    if password.is_empty() {
        cache_remove("password", host_id);
        db.delete_credential(host_id, "password")
            .map_err(|e| format!("删除凭据失败: {e}"))?;
        return Ok(());
    }
    let enc = encrypt_secret(password)?;
    db.set_credential(host_id, "password", &enc)
        .map_err(|e| format!("保存凭据到数据库失败: {e}"))?;
    cache_set("password", host_id, password.to_string());
    Ok(())
}

pub fn get_password(host_id: &str) -> Option<String> {
    if let Some(cached) = cache_get("password", host_id) {
        return Some(cached);
    }
    let db = db()?;
    let enc = db.get_credential(host_id, "password").ok()??;
    let plain = decrypt_secret(&enc).ok()?;
    cache_set("password", host_id, plain.clone());
    Some(plain)
}

pub fn delete_password(host_id: &str) {
    cache_remove("password", host_id);
    if let Some(db) = db() {
        let _ = db.delete_credential(host_id, "password");
    }
}

// ---------- AI API Key ----------

pub fn save_api_key(provider_id: &str, key: &str) -> Result<(), String> {
    let db = db().ok_or_else(|| "数据库未就绪".to_string())?;
    if key.is_empty() {
        cache_remove("apikey", provider_id);
        db.delete_credential(provider_id, "apikey")
            .map_err(|e| format!("删除 API Key 失败: {e}"))?;
        return Ok(());
    }
    let enc = encrypt_secret(key)?;
    db.set_credential(provider_id, "apikey", &enc)
        .map_err(|e| format!("保存 API Key 到数据库失败: {e}"))?;
    cache_set("apikey", provider_id, key.to_string());
    Ok(())
}

pub fn get_api_key(provider_id: &str) -> Option<String> {
    if let Some(cached) = cache_get("apikey", provider_id) {
        return Some(cached);
    }
    let db = db()?;
    let enc = db.get_credential(provider_id, "apikey").ok()??;
    let plain = decrypt_secret(&enc).ok()?;
    cache_set("apikey", provider_id, plain.clone());
    Some(plain)
}

pub fn delete_api_key(provider_id: &str) {
    cache_remove("apikey", provider_id);
    if let Some(db) = db() {
        let _ = db.delete_credential(provider_id, "apikey");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let plain = "MyS3cret!密码123";
        let enc = encrypt_secret(plain).expect("encrypt 失败");
        assert_ne!(enc, plain, "密文不应等于明文");
        let dec = decrypt_secret(&enc).expect("decrypt 失败");
        assert_eq!(dec, plain);
    }

    #[test]
    fn different_plaintext_same_key_differs() {
        let a = encrypt_secret("abc").unwrap();
        let b = encrypt_secret("abc").unwrap();
        assert_ne!(a, b, "随机 nonce 应使同明文密文不同");
        assert_eq!(decrypt_secret(&a).unwrap(), "abc");
        assert_eq!(decrypt_secret(&b).unwrap(), "abc");
    }

    #[test]
    fn decrypt_garbage_fails() {
        assert!(decrypt_secret("not-base64!!").is_err());
        assert!(decrypt_secret("c2hvcnQ=").is_err(), "过短密文应失败");
    }
}
