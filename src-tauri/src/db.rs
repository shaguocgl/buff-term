use crate::models::{
    AiModel, AiProvider, AiRule, AlertRule, AlertSettings, AuditLog, Host, Inspection,
    InspectionRun, McpServer,
};
use rusqlite::{params, Connection, Row};
use rusqlite::OptionalExtension;
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
                 secret         TEXT,
                 cooldown_min   INTEGER NOT NULL DEFAULT 10,
                 enabled        INTEGER NOT NULL DEFAULT 1,
                 created_at     INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS inspections (
                 id             TEXT PRIMARY KEY,
                 host_id        TEXT NOT NULL,
                 interval_min   INTEGER NOT NULL DEFAULT 60,
                 enabled        INTEGER NOT NULL DEFAULT 1,
                 last_run_at    INTEGER,
                 created_at     INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS inspection_runs (
                 id             TEXT PRIMARY KEY,
                 inspection_id  TEXT NOT NULL,
                 host_id        TEXT NOT NULL,
                 host_label     TEXT NOT NULL,
                 started_at     INTEGER NOT NULL,
                 finished_at    INTEGER,
                 status         TEXT NOT NULL,
                 risk_level     TEXT NOT NULL DEFAULT 'low',
                 summary        TEXT,
                 respond_text   TEXT
             );
             CREATE TABLE IF NOT EXISTS mcp_servers (
                 id         TEXT PRIMARY KEY,
                 name       TEXT NOT NULL,
                 command    TEXT NOT NULL,
                 args       TEXT NOT NULL DEFAULT '',
                 enabled    INTEGER NOT NULL DEFAULT 1,
                 created_at INTEGER NOT NULL
             );",
        )?;
        migrate_ai_models(&conn)?;
        migrate_alerts_secret(&conn)?;
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
            "INSERT INTO ai_providers (id, name, base_url, model, protocol, enabled, created_at)
             VALUES (?1, ?2, ?3, '', ?4, ?5, ?6)",
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

    pub fn set_ai_provider_enabled(&self, id: &str, enabled: bool) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE ai_providers SET enabled=?2 WHERE id=?1",
            params![id, enabled as i64],
        )?;
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
            "SELECT id, metric, operator, threshold, channel, target, secret, cooldown_min, enabled, created_at
             FROM alerts WHERE enabled=1 ORDER BY created_at DESC"
        } else {
            "SELECT id, metric, operator, threshold, channel, target, secret, cooldown_min, enabled, created_at
             FROM alerts ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_alert)?;
        rows.collect()
    }

    pub fn insert_alert(&self, rule: &AlertRule) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO alerts (id, metric, operator, threshold, channel, target, secret, cooldown_min, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                rule.id,
                rule.metric,
                rule.operator,
                rule.threshold,
                rule.channel,
                rule.target,
                rule.secret,
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
                    target=?6, secret=?7, cooldown_min=?8, enabled=?9
             WHERE id=?1",
            params![
                rule.id,
                rule.metric,
                rule.operator,
                rule.threshold,
                rule.channel,
                rule.target,
                rule.secret,
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

    pub fn get_alert_settings(&self) -> rusqlite::Result<AlertSettings> {
        let conn = self.conn.lock().unwrap();
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='alert_settings'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match value {
            Some(v) => serde_json::from_str(&v).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            }),
            None => Ok(AlertSettings::default()),
        }
    }

    pub fn save_alert_settings(&self, settings: &AlertSettings) -> rusqlite::Result<()> {
        let value = serde_json::to_string(settings)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.set_setting("alert_settings", &value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_host(&self, id: &str) -> rusqlite::Result<Option<Host>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, address, port, username, auth_type, key_path, proxy_jump, notes, created_at
             FROM hosts WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_host)?;
        match rows.next() {
            Some(Ok(host)) => Ok(Some(host)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn list_inspections(&self) -> rusqlite::Result<Vec<Inspection>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, host_id, interval_min, enabled, last_run_at, created_at
             FROM inspections ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_inspection)?;
        rows.collect()
    }

    pub fn insert_inspection(&self, i: &Inspection) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO inspections (id, host_id, interval_min, enabled, last_run_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                i.id,
                i.host_id,
                i.interval_min as i64,
                i.enabled as i64,
                i.last_run_at.map(|v| v as i64),
                i.created_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update_inspection(&self, i: &Inspection) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE inspections SET interval_min=?2, enabled=?3 WHERE id=?1",
            params![i.id, i.interval_min as i64, i.enabled as i64],
        )?;
        Ok(())
    }

    pub fn delete_inspection(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM inspections WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn set_inspection_last_run(&self, id: &str, ts: u64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE inspections SET last_run_at=?2 WHERE id=?1",
            params![id, ts as i64],
        )?;
        Ok(())
    }

    pub fn insert_inspection_run(&self, r: &InspectionRun) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO inspection_runs (id, inspection_id, host_id, host_label, started_at,
                                          finished_at, status, risk_level, summary, respond_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                r.id,
                r.inspection_id,
                r.host_id,
                r.host_label,
                r.started_at as i64,
                r.finished_at.map(|v| v as i64),
                r.status,
                r.risk_level,
                r.summary,
                r.respond_text,
            ],
        )?;
        Ok(())
    }

    pub fn update_inspection_run(&self, r: &InspectionRun) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE inspection_runs SET finished_at=?2, status=?3, risk_level=?4,
                    summary=?5, respond_text=?6
             WHERE id=?1",
            params![
                r.id,
                r.finished_at.map(|v| v as i64),
                r.status,
                r.risk_level,
                r.summary,
                r.respond_text,
            ],
        )?;
        Ok(())
    }

    pub fn list_inspection_runs(&self, limit: u32) -> rusqlite::Result<Vec<InspectionRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, inspection_id, host_id, host_label, started_at, finished_at,
                    status, risk_level, summary, respond_text
             FROM inspection_runs ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_inspection_run)?;
        rows.collect()
    }

    pub fn get_inspection_run(&self, id: &str) -> rusqlite::Result<Option<InspectionRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, inspection_id, host_id, host_label, started_at, finished_at,
                    status, risk_level, summary, respond_text
             FROM inspection_runs WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_inspection_run)?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn list_mcp_servers(&self, enabled_only: bool) -> rusqlite::Result<Vec<McpServer>> {
        let conn = self.conn.lock().unwrap();
        let sql = if enabled_only {
            "SELECT id, name, command, args, enabled, created_at
             FROM mcp_servers WHERE enabled=1 ORDER BY created_at DESC"
        } else {
            "SELECT id, name, command, args, enabled, created_at
             FROM mcp_servers ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_mcp)?;
        rows.collect()
    }

    pub fn insert_mcp_server(&self, s: &McpServer) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mcp_servers (id, name, command, args, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                s.id,
                s.name,
                s.command,
                s.args,
                s.enabled as i64,
                s.created_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update_mcp_server(&self, s: &McpServer) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE mcp_servers SET name=?2, command=?3, args=?4, enabled=?5 WHERE id=?1",
            params![s.id, s.name, s.command, s.args, s.enabled as i64],
        )?;
        Ok(())
    }

    pub fn delete_mcp_server(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM mcp_servers WHERE id=?1", params![id])?;
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
        secret: row.get(6)?,
        cooldown_min: row.get::<_, i64>(7)? as u64,
        enabled: row.get::<_, i64>(8)? != 0,
        created_at: row.get::<_, i64>(9)? as u64,
    })
}

/// 旧版 alerts 表没有 secret 列，这里做增量迁移
fn migrate_alerts_secret(conn: &Connection) -> rusqlite::Result<()> {
    let has_secret: bool = {
        let mut stmt = conn.prepare("PRAGMA table_info(alerts)")?;
        let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = false;
        for col in cols {
            if col? == "secret" {
                found = true;
            }
        }
        found
    };
    if !has_secret {
        conn.execute_batch("ALTER TABLE alerts ADD COLUMN secret TEXT;")?;
    }
    Ok(())
}


fn row_to_inspection(row: &Row<'_>) -> rusqlite::Result<Inspection> {
    Ok(Inspection {
        id: row.get(0)?,
        host_id: row.get(1)?,
        interval_min: row.get::<_, i64>(2)? as u64,
        enabled: row.get::<_, i64>(3)? != 0,
        last_run_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
        created_at: row.get::<_, i64>(5)? as u64,
    })
}

fn row_to_inspection_run(row: &Row<'_>) -> rusqlite::Result<InspectionRun> {
    Ok(InspectionRun {
        id: row.get(0)?,
        inspection_id: row.get(1)?,
        host_id: row.get(2)?,
        host_label: row.get(3)?,
        started_at: row.get::<_, i64>(4)? as u64,
        finished_at: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        status: row.get(6)?,
        risk_level: row.get(7)?,
        summary: row.get(8)?,
        respond_text: row.get(9)?,
    })
}

fn row_to_mcp(row: &Row<'_>) -> rusqlite::Result<McpServer> {
    Ok(McpServer {
        id: row.get(0)?,
        name: row.get(1)?,
        command: row.get(2)?,
        args: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        created_at: row.get::<_, i64>(5)? as u64,
    })
}
