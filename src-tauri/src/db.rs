use crate::models::{AiModel, AiProvider, AiRule, AlertRule, AuditLog, Host};
use rusqlite::{params, Connection, Row};
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS hosts (
                 id          TEXT PRIMARY KEY,
                 name        TEXT NOT NULL,
                 address     TEXT NOT NULL,
                 port        INTEGER NOT NULL DEFAULT 22,
                 username    TEXT NOT NULL,
                 auth_type   TEXT NOT NULL DEFAULT 'key',
                 key_path    TEXT,
                 proxy_jump  TEXT,
                 notes       TEXT,
                 created_at  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ai_providers (
                 id         TEXT PRIMARY KEY,
                 name       TEXT NOT NULL,
                 base_url   TEXT NOT NULL,
                 model      TEXT NOT NULL DEFAULT '',
                 protocol   TEXT NOT NULL DEFAULT 'openai-compatible',
                 enabled    INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ai_models (
                 id          TEXT PRIMARY KEY,
                 provider_id TEXT NOT NULL,
                 label       TEXT NOT NULL,
                 model       TEXT NOT NULL,
                 is_active   INTEGER NOT NULL DEFAULT 0,
                 sort        INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS ai_rules (
                 id         TEXT PRIMARY KEY,
                 pattern    TEXT NOT NULL,
                 enabled    INTEGER NOT NULL DEFAULT 1,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS audit_logs (
                 id             TEXT PRIMARY KEY,
                 ts             INTEGER NOT NULL,
                 session_id     INTEGER,
                 host_id        TEXT NOT NULL,
                 host_label     TEXT NOT NULL,
                 tool_name      TEXT NOT NULL,
                 summary        TEXT NOT NULL,
                 permission_mode TEXT NOT NULL,
                 approval       TEXT NOT NULL,
                 status         TEXT NOT NULL,
                 result         TEXT,
                 duration_ms    INTEGER
             );
             CREATE TABLE IF NOT EXISTS alerts (
                 id             TEXT PRIMARY KEY,
                 metric         TEXT NOT NULL,
                 operator       TEXT NOT NULL,
                 threshold      REAL NOT NULL,
                 channel        TEXT NOT NULL,
                 target         TEXT,
                 cooldown_min   INTEGER NOT NULL DEFAULT 10,
                 enabled        INTEGER NOT NULL DEFAULT 1,
                 created_at     INTEGER NOT NULL
             );",
        )?;
        migrate_ai_models(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn list(&self) -> rusqlite::Result<Vec<Host>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, address, port, username, auth_type, key_path, proxy_jump, notes, created_at
             FROM hosts ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_host)?;
        rows.collect()
    }

    pub fn insert(&self, host: &Host) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO hosts (id, name, address, port, username, auth_type, key_path, proxy_jump, notes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                host.id,
                host.name,
                host.address,
                host.port as i64,
                host.username,
                host.auth_type,
                host.key_path,
                host.proxy_jump,
                host.notes,
                host.created_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update(&self, host: &Host) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE hosts SET name=?2, address=?3, port=?4, username=?5, auth_type=?6,
                    key_path=?7, proxy_jump=?8, notes=?9
             WHERE id=?1",
            params![
                host.id,
                host.name,
                host.address,
                host.port as i64,
                host.username,
                host.auth_type,
                host.key_path,
                host.proxy_jump,
                host.notes,
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM hosts WHERE id=?1", params![id])?;
        Ok(())
    }

    // ---------- AI 配置 ----------

    pub fn list_ai_providers(&self) -> rusqlite::Result<Vec<AiProvider>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, base_url, protocol, enabled, created_at
             FROM ai_providers ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_ai)?;
        let mut providers: Vec<AiProvider> = rows.collect::<rusqlite::Result<_>>()?;
        for provider in &mut providers {
            provider.models = self.models_of(&conn, &provider.id)?;
        }
        Ok(providers)
    }

    fn models_of(
        &self,
        conn: &Connection,
        provider_id: &str,
    ) -> rusqlite::Result<Vec<AiModel>> {
        let mut stmt = conn.prepare(
            "SELECT id, label, model, is_active FROM ai_models
             WHERE provider_id=?1 ORDER BY sort, rowid",
        )?;
        let rows = stmt.query_map(params![provider_id], row_to_ai_model)?;
        rows.collect()
    }

    pub fn insert_ai_provider(&self, p: &AiProvider) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ai_providers (id, name, base_url, protocol, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                p.id,
                p.name,
                p.base_url,
                p.protocol,
                p.enabled as i64,
                p.created_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update_ai_provider(&self, p: &AiProvider) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ai_providers SET name=?2, base_url=?3, protocol=?4, enabled=?5
             WHERE id=?1",
            params![p.id, p.name, p.base_url, p.protocol, p.enabled as i64],
        )?;
        Ok(())
    }

    pub fn disable_all_ai_providers(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE ai_providers SET enabled=0", [])?;
        Ok(())
    }

    pub fn replace_ai_models(
        &self,
        provider_id: &str,
        models: &[AiModel],
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM ai_models WHERE provider_id=?1",
            params![provider_id],
        )?;
        for (idx, m) in models.iter().enumerate() {
            conn.execute(
                "INSERT INTO ai_models (id, provider_id, label, model, is_active, sort)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    m.id,
                    provider_id,
                    m.label,
                    m.model,
                    m.is_active as i64,
                    idx as i64,
                ],
            )?;
        }
        Ok(())
    }

    pub fn set_active_ai_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ai_models SET is_active=0 WHERE provider_id=?1",
            params![provider_id],
        )?;
        conn.execute(
            "UPDATE ai_models SET is_active=1 WHERE id=?1 AND provider_id=?2",
            params![model_id, provider_id],
        )?;
        Ok(())
    }

    pub fn delete_ai_provider(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM ai_models WHERE provider_id=?1", params![id])?;
        conn.execute("DELETE FROM ai_providers WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn list_ai_rules(&self) -> rusqlite::Result<Vec<AiRule>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pattern, enabled, created_at FROM ai_rules
             WHERE enabled=1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_ai_rule)?;
        rows.collect()
    }

    pub fn insert_ai_rule(&self, rule: &AiRule) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ai_rules (id, pattern, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![rule.id, rule.pattern, rule.enabled as i64, rule.created_at as i64],
        )?;
        Ok(())
    }

    pub fn delete_ai_rule(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM ai_rules WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn insert_audit_log(&self, log: &AuditLog) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audit_logs (id, ts, session_id, host_id, host_label, tool_name, summary,
                                     permission_mode, approval, status, result, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                log.id,
                log.ts as i64,
                log.session_id.map(|v| v as i64),
                log.host_id,
                log.host_label,
                log.tool_name,
                log.summary,
                log.permission_mode,
                log.approval,
                log.status,
                log.result,
                log.duration_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn list_audit_logs(&self, limit: u32) -> rusqlite::Result<Vec<AuditLog>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, session_id, host_id, host_label, tool_name, summary,
                    permission_mode, approval, status, result, duration_ms
             FROM audit_logs ORDER BY ts DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_audit)?;
        rows.collect()
    }

    pub fn list_alerts(&self, enabled_only: bool) -> rusqlite::Result<Vec<AlertRule>> {
        let conn = self.conn.lock().unwrap();
        let sql = if enabled_only {
            "SELECT id, metric, operator, threshold, channel, target, cooldown_min, enabled, created_at
             FROM alerts WHERE enabled=1 ORDER BY created_at DESC"
        } else {
            "SELECT id, metric, operator, threshold, channel, target, cooldown_min, enabled, created_at
             FROM alerts ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_alert)?;
        rows.collect()
    }

    pub fn insert_alert(&self, rule: &AlertRule) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO alerts (id, metric, operator, threshold, channel, target, cooldown_min, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                rule.id,
                rule.metric,
                rule.operator,
                rule.threshold,
                rule.channel,
                rule.target,
                rule.cooldown_min as i64,
                rule.enabled as i64,
                rule.created_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update_alert(&self, rule: &AlertRule) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE alerts SET metric=?2, operator=?3, threshold=?4, channel=?5,
                    target=?6, cooldown_min=?7, enabled=?8
             WHERE id=?1",
            params![
                rule.id,
                rule.metric,
                rule.operator,
                rule.threshold,
                rule.channel,
                rule.target,
                rule.cooldown_min as i64,
                rule.enabled as i64,
            ],
        )?;
        Ok(())
    }

    pub fn delete_alert(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM alerts WHERE id=?1", params![id])?;
        Ok(())
    }
}

/// 旧版本只有一个 model 字段，迁移到 ai_models 表
fn migrate_ai_models(conn: &Connection) -> rusqlite::Result<()> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ai_models",
        [],
        |row| row.get(0),
    )?;
    if count > 0 {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT id, model FROM ai_providers WHERE model IS NOT NULL AND model != ''",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (provider_id, model) in rows {
        conn.execute(
            "INSERT INTO ai_models (id, provider_id, label, model, is_active, sort)
             VALUES (?1, ?2, ?3, ?4, 1, 0)",
            params![
                uuid::Uuid::new_v4().to_string(),
                provider_id,
                model,
                model,
            ],
        )?;
    }
    Ok(())
}

fn row_to_host(row: &Row<'_>) -> rusqlite::Result<Host> {
    Ok(Host {
        id: row.get(0)?,
        name: row.get(1)?,
        address: row.get(2)?,
        port: row.get::<_, i64>(3)? as u16,
        username: row.get(4)?,
        auth_type: row.get(5)?,
        key_path: row.get(6)?,
        proxy_jump: row.get(7)?,
        notes: row.get(8)?,
        created_at: row.get::<_, i64>(9)? as u64,
    })
}

fn row_to_ai(row: &Row<'_>) -> rusqlite::Result<AiProvider> {
    Ok(AiProvider {
        id: row.get(0)?,
        name: row.get(1)?,
        base_url: row.get(2)?,
        protocol: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        created_at: row.get::<_, i64>(5)? as u64,
        models: Vec::new(),
    })
}

fn row_to_ai_model(row: &Row<'_>) -> rusqlite::Result<AiModel> {
    Ok(AiModel {
        id: row.get(0)?,
        label: row.get(1)?,
        model: row.get(2)?,
        is_active: row.get::<_, i64>(3)? != 0,
    })
}

fn row_to_ai_rule(row: &Row<'_>) -> rusqlite::Result<AiRule> {
    Ok(AiRule {
        id: row.get(0)?,
        pattern: row.get(1)?,
        enabled: row.get::<_, i64>(2)? != 0,
        created_at: row.get::<_, i64>(3)? as u64,
    })
}

fn row_to_audit(row: &Row<'_>) -> rusqlite::Result<AuditLog> {
    Ok(AuditLog {
        id: row.get(0)?,
        ts: row.get::<_, i64>(1)? as u64,
        session_id: row.get::<_, Option<i64>>(2)?.map(|v| v as u32),
        host_id: row.get(3)?,
        host_label: row.get(4)?,
        tool_name: row.get(5)?,
        summary: row.get(6)?,
        permission_mode: row.get(7)?,
        approval: row.get(8)?,
        status: row.get(9)?,
        result: row.get(10)?,
        duration_ms: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
    })
}

fn row_to_alert(row: &Row<'_>) -> rusqlite::Result<AlertRule> {
    Ok(AlertRule {
        id: row.get(0)?,
        metric: row.get(1)?,
        operator: row.get(2)?,
        threshold: row.get(3)?,
        channel: row.get(4)?,
        target: row.get(5)?,
        cooldown_min: row.get::<_, i64>(6)? as u64,
        enabled: row.get::<_, i64>(7)? != 0,
        created_at: row.get::<_, i64>(8)? as u64,
    })
}
