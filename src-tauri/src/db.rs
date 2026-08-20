use crate::models::{
    AiModel, AiProvider, AiRule, AlertSettings, AuditLog, AuthType, Host, HostMetric, InspectionReport,
    McpPermissionMode, McpRule, McpService, MetricDisk, MetricTop, NewMetric, Remediation, TerminalGuardSettings, TerminalRule,
};
use crate::util::now;
use rusqlite::{params, Connection, Row};
use rusqlite::OptionalExtension;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub struct Db {
    conn: Mutex<Connection>,
}

/// 全局数据库入口：供凭据解密等非 Tauri 上下文直接读取数据库。
static GLOBAL_DB: OnceLock<Arc<Db>> = OnceLock::new();

pub fn init_global(db: Arc<Db>) {
    let _ = GLOBAL_DB.set(db);
}

pub fn global() -> Option<&'static Arc<Db>> {
    GLOBAL_DB.get()
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
                 notes       TEXT,
                 created_at  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ai_providers (
                 id         TEXT PRIMARY KEY,
                 name       TEXT NOT NULL,
                 base_url   TEXT NOT NULL,
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
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mcp_service (
                 id             INTEGER PRIMARY KEY CHECK (id = 1),
                 enabled        INTEGER NOT NULL DEFAULT 0,
                 host_ids       TEXT NOT NULL DEFAULT '[]',
                 permission_mode TEXT NOT NULL DEFAULT 'confirm',
                 token          TEXT,
                 port           INTEGER,
                 updated_at     INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS mcp_rules (
                 id         TEXT PRIMARY KEY,
                 pattern    TEXT NOT NULL,
                 enabled    INTEGER NOT NULL DEFAULT 1,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS terminal_rules (
                 id         TEXT PRIMARY KEY,
                 pattern    TEXT NOT NULL,
                 enabled    INTEGER NOT NULL DEFAULT 1,
                 builtin    INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS inspection_reports (
                 id            TEXT PRIMARY KEY,
                 host_id       TEXT NOT NULL,
                 host_label    TEXT NOT NULL,
                 provider_id   TEXT NOT NULL,
                 provider_name TEXT NOT NULL,
                 model         TEXT NOT NULL,
                 status        TEXT NOT NULL,
                 risk_level    TEXT NOT NULL DEFAULT 'unknown',
                 summary       TEXT NOT NULL DEFAULT '',
                 markdown      TEXT NOT NULL DEFAULT '',
                 html          TEXT NOT NULL DEFAULT '',
                 email_sent    INTEGER NOT NULL DEFAULT 0,
                 error         TEXT,
                 created_at    INTEGER NOT NULL,
                 finished_at   INTEGER,
                 duration_ms   INTEGER
             );
             CREATE TABLE IF NOT EXISTS remediations (
                 id             TEXT PRIMARY KEY,
                 report_id      TEXT NOT NULL UNIQUE,
                 host_id        TEXT NOT NULL,
                 host_label     TEXT NOT NULL,
                 provider_id    TEXT NOT NULL,
                 provider_name  TEXT NOT NULL,
                 model          TEXT NOT NULL,
                 intervention   TEXT NOT NULL DEFAULT '',
                 plan_markdown  TEXT NOT NULL DEFAULT '',
                 steps_json     TEXT NOT NULL DEFAULT '[]',
                 status         TEXT NOT NULL DEFAULT 'draft',
                 error          TEXT,
                 created_at     INTEGER NOT NULL,
                 started_at     INTEGER,
                 finished_at    INTEGER,
                 duration_ms    INTEGER
             );
             CREATE TABLE IF NOT EXISTS credentials (
                 owner_id   TEXT NOT NULL,
                 kind       TEXT NOT NULL,
                 secret_enc TEXT NOT NULL,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (owner_id, kind)
             );
             CREATE TABLE IF NOT EXISTS host_metrics (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 host_id      TEXT NOT NULL,
                 ts           INTEGER NOT NULL,
                 cpu_percent  REAL NOT NULL DEFAULT 0,
                 load1        REAL NOT NULL DEFAULT 0,
                 mem_total_mb INTEGER NOT NULL DEFAULT 0,
                 mem_used_mb  INTEGER NOT NULL DEFAULT 0,
                 mem_percent  REAL NOT NULL DEFAULT 0,
                 disks_json   TEXT NOT NULL DEFAULT '[]',
                 top_json     TEXT NOT NULL DEFAULT '[]',
                 source       TEXT NOT NULL DEFAULT 'manual'
             );
             CREATE INDEX IF NOT EXISTS idx_metrics_host_ts ON host_metrics(host_id, ts DESC);",
        )?;

        // 首次建库时写入终端防护预置规则（含删除后重启不重复恢复的标记）
        let seeded: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='terminal_rules_seeded'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if seeded.is_none() {
            for (i, p) in crate::guard::PRESET_TERMINAL_RULES.iter().enumerate() {
                conn.execute(
                    "INSERT INTO terminal_rules (id, pattern, enabled, builtin, created_at)
                     VALUES (?1, ?2, 1, 1, ?3)",
                    params![format!("builtin-{i}"), p, now()],
                )?;
            }
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('terminal_rules_seeded', '1')
                 ON CONFLICT(key) DO UPDATE SET value='1'",
                [],
            )?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn list(&self) -> rusqlite::Result<Vec<Host>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, address, port, username, auth_type, key_path, notes, created_at
             FROM hosts ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_host)?;
        rows.collect()
    }

    pub fn get_host(&self, id: &str) -> rusqlite::Result<Option<Host>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, address, port, username, auth_type, key_path, notes, created_at
             FROM hosts WHERE id=?1",
            params![id],
            row_to_host,
        )
        .optional()
    }

    pub fn insert(&self, host: &Host) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO hosts (id, name, address, port, username, auth_type, key_path, notes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                host.id,
                host.name,
                host.address,
                host.port as i64,
                host.username,
                host.auth_type.as_str(),
                host.key_path,
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
                    key_path=?7, notes=?8
             WHERE id=?1",
            params![
                host.id,
                host.name,
                host.address,
                host.port as i64,
                host.username,
                host.auth_type.as_str(),
                host.key_path,
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

    pub fn get_mcp_service(&self) -> rusqlite::Result<McpService> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT enabled, host_ids, permission_mode, token, port, updated_at
                 FROM mcp_service WHERE id=1",
                [],
                |row| {
                    Ok(McpService {
                        enabled: row.get::<_, i64>(0)? != 0,
                        host_ids: serde_json::from_str(&row.get::<_, String>(1)?)
                            .unwrap_or_default(),
                        permission_mode: row
                            .get::<_, String>(2)?
                            .parse()
                            .unwrap_or(McpPermissionMode::Confirm),
                        token: row.get(3)?,
                        port: row.get(4)?,
                        updated_at: row.get::<_, i64>(5)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or(McpService {
            enabled: false,
            host_ids: Vec::new(),
            permission_mode: McpPermissionMode::Confirm,
            token: None,
            port: None,
            updated_at: 0,
        }))
    }

    pub fn save_mcp_service(&self, s: &McpService) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mcp_service (id, enabled, host_ids, permission_mode, token, port, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               enabled=?1, host_ids=?2, permission_mode=?3, token=?4, port=?5, updated_at=?6",
            params![
                s.enabled as i64,
                serde_json::to_string(&s.host_ids).unwrap_or_else(|_| "[]".to_string()),
                s.permission_mode.as_str(),
                s.token,
                s.port.map(|p| p as i64),
                s.updated_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn list_mcp_rules(&self) -> rusqlite::Result<Vec<McpRule>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pattern, enabled, created_at FROM mcp_rules
             WHERE enabled=1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_mcp_rule)?;
        rows.collect()
    }

    pub fn insert_mcp_rule(&self, rule: &McpRule) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mcp_rules (id, pattern, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![rule.id, rule.pattern, rule.enabled as i64, rule.created_at as i64],
        )?;
        Ok(())
    }

    pub fn delete_mcp_rule(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM mcp_rules WHERE id=?1", params![id])?;
        Ok(())
    }

    // ---------- 终端危险命令拦截规则 ----------

    pub fn list_terminal_rules(&self) -> rusqlite::Result<Vec<TerminalRule>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pattern, enabled, builtin, created_at FROM terminal_rules
             ORDER BY builtin DESC, created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_terminal_rule)?;
        rows.collect()
    }

    pub fn insert_terminal_rule(&self, rule: &TerminalRule) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO terminal_rules (id, pattern, enabled, builtin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                rule.id,
                rule.pattern,
                rule.enabled as i64,
                rule.builtin as i64,
                rule.created_at as i64,
            ],
        )?;
        Ok(())
    }

    pub fn delete_terminal_rule(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM terminal_rules WHERE id=?1", params![id])?;
        Ok(())
    }

    /// 删除全部预置规则并按当前预置清单重新写入（自定义规则保留）。
    pub fn reset_terminal_rules(&self) -> rusqlite::Result<Vec<TerminalRule>> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM terminal_rules WHERE builtin=1", [])?;
            for (i, p) in crate::guard::PRESET_TERMINAL_RULES.iter().enumerate() {
                conn.execute(
                    "INSERT INTO terminal_rules (id, pattern, enabled, builtin, created_at)
                     VALUES (?1, ?2, 1, 1, ?3)",
                    params![format!("builtin-{i}"), p, now()],
                )?;
            }
        }
        self.list_terminal_rules()
    }

    pub fn get_terminal_guard_settings(&self) -> rusqlite::Result<TerminalGuardSettings> {
        let conn = self.conn.lock().unwrap();
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='terminal_guard_settings'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match value {
            Some(v) => serde_json::from_str(&v).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            }),
            None => Ok(TerminalGuardSettings::default()),
        }
    }

    pub fn save_terminal_guard_settings(
        &self,
        settings: &TerminalGuardSettings,
    ) -> rusqlite::Result<()> {
        let value = serde_json::to_string(settings)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.set_setting("terminal_guard_settings", &value)
    }

    /// 保存加密后的凭据（主机密码 / AI API Key 等）。
    pub fn set_credential(&self, owner_id: &str, kind: &str, secret_enc: &str) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO credentials (owner_id, kind, secret_enc, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(owner_id, kind) DO UPDATE SET secret_enc = ?3, updated_at = ?4",
            params![owner_id, kind, secret_enc, now()],
        )?;
        Ok(())
    }

    pub fn get_credential(&self, owner_id: &str, kind: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT secret_enc FROM credentials WHERE owner_id = ?1 AND kind = ?2",
            params![owner_id, kind],
            |row| row.get(0),
        )
        .optional()
    }

    pub fn delete_credential(&self, owner_id: &str, kind: &str) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM credentials WHERE owner_id = ?1 AND kind = ?2",
            params![owner_id, kind],
        )?;
        Ok(())
    }

    pub fn insert_inspection_report(&self, report: &InspectionReport) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO inspection_reports (
                 id, host_id, host_label, provider_id, provider_name, model, status,
                 risk_level, summary, markdown, html, email_sent, error,
                 created_at, finished_at, duration_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
             )",
            params![
                report.id,
                report.host_id,
                report.host_label,
                report.provider_id,
                report.provider_name,
                report.model,
                report.status,
                report.risk_level,
                report.summary,
                report.markdown,
                report.html,
                report.email_sent as i64,
                report.error,
                report.created_at as i64,
                report.finished_at.map(|v| v as i64),
                report.duration_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn update_inspection_report(&self, report: &InspectionReport) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE inspection_reports SET
                status=?2, risk_level=?3, summary=?4, markdown=?5, html=?6,
                email_sent=?7, error=?8, finished_at=?9, duration_ms=?10
             WHERE id=?1",
            params![
                report.id,
                report.status,
                report.risk_level,
                report.summary,
                report.markdown,
                report.html,
                report.email_sent as i64,
                report.error,
                report.finished_at.map(|v| v as i64),
                report.duration_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn get_inspection_report(&self, id: &str) -> rusqlite::Result<Option<InspectionReport>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, host_id, host_label, provider_id, provider_name, model, status,
                    risk_level, summary, markdown, html, email_sent, error,
                    created_at, finished_at, duration_ms
             FROM inspection_reports WHERE id=?1",
            params![id],
            row_to_inspection_report,
        )
        .optional()
    }

    pub fn list_inspection_reports(
        &self,
        host_id: Option<&str>,
        limit: u32,
    ) -> rusqlite::Result<Vec<InspectionReport>> {
        let conn = self.conn.lock().unwrap();
        let rows = match host_id {
            Some(host_id) => {
                let mut stmt = conn.prepare(
                    "SELECT id, host_id, host_label, provider_id, provider_name, model, status,
                            risk_level, summary, markdown, html, email_sent, error,
                            created_at, finished_at, duration_ms
                     FROM inspection_reports WHERE host_id=?1
                     ORDER BY created_at DESC, rowid DESC LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![host_id, limit as i64], row_to_inspection_report)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, host_id, host_label, provider_id, provider_name, model, status,
                            risk_level, summary, markdown, html, email_sent, error,
                            created_at, finished_at, duration_ms
                     FROM inspection_reports ORDER BY created_at DESC, rowid DESC LIMIT ?1",
                )?;
                let rows = stmt
                    .query_map(params![limit as i64], row_to_inspection_report)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
        };
        Ok(rows)
    }

    pub fn delete_inspection_report(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM inspection_reports WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn upsert_remediation(&self, remediation: &Remediation) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let steps_json = serde_json::to_string(&remediation.steps)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        conn.execute(
            "INSERT INTO remediations (
                 id, report_id, host_id, host_label, provider_id, provider_name, model,
                 intervention, plan_markdown, steps_json, status, error,
                 created_at, started_at, finished_at, duration_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
             )
             ON CONFLICT(report_id) DO UPDATE SET
                 id=excluded.id,
                 host_id=excluded.host_id,
                 host_label=excluded.host_label,
                 provider_id=excluded.provider_id,
                 provider_name=excluded.provider_name,
                 model=excluded.model,
                 intervention=excluded.intervention,
                 plan_markdown=excluded.plan_markdown,
                 steps_json=excluded.steps_json,
                 status=excluded.status,
                 error=excluded.error,
                 created_at=excluded.created_at,
                 started_at=excluded.started_at,
                 finished_at=excluded.finished_at,
                 duration_ms=excluded.duration_ms",
            params![
                remediation.id,
                remediation.report_id,
                remediation.host_id,
                remediation.host_label,
                remediation.provider_id,
                remediation.provider_name,
                remediation.model,
                remediation.intervention,
                remediation.plan_markdown,
                steps_json,
                remediation.status,
                remediation.error,
                remediation.created_at as i64,
                remediation.started_at.map(|v| v as i64),
                remediation.finished_at.map(|v| v as i64),
                remediation.duration_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn update_remediation(&self, remediation: &Remediation) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let steps_json = serde_json::to_string(&remediation.steps)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        conn.execute(
            "UPDATE remediations SET
                 plan_markdown=?2, steps_json=?3, status=?4, error=?5,
                 started_at=?6, finished_at=?7, duration_ms=?8
             WHERE id=?1",
            params![
                remediation.id,
                remediation.plan_markdown,
                steps_json,
                remediation.status,
                remediation.error,
                remediation.started_at.map(|v| v as i64),
                remediation.finished_at.map(|v| v as i64),
                remediation.duration_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn get_remediation(&self, id: &str) -> rusqlite::Result<Option<Remediation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, report_id, host_id, host_label, provider_id, provider_name, model,
                    intervention, plan_markdown, steps_json, status, error,
                    created_at, started_at, finished_at, duration_ms
             FROM remediations WHERE id=?1",
            params![id],
            row_to_remediation,
        )
        .optional()
    }

    pub fn get_remediation_by_report(
        &self,
        report_id: &str,
    ) -> rusqlite::Result<Option<Remediation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, report_id, host_id, host_label, provider_id, provider_name, model,
                    intervention, plan_markdown, steps_json, status, error,
                    created_at, started_at, finished_at, duration_ms
             FROM remediations WHERE report_id=?1",
            params![report_id],
            row_to_remediation,
        )
        .optional()
    }

    // ============ 主机历史指标 ============

    /// 插入一条主机指标快照。disks/top 以 JSON 形式存储。
    pub fn insert_metric(&self, metric: NewMetric<'_>) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO host_metrics (
                host_id, ts, cpu_percent, load1, mem_total_mb, mem_used_mb,
                mem_percent, disks_json, top_json, source
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                metric.host_id,
                metric.ts as i64,
                metric.cpu_percent,
                metric.load1,
                metric.mem_total_mb as i64,
                metric.mem_used_mb as i64,
                metric.mem_percent,
                metric.disks_json,
                metric.top_json,
                metric.source,
            ],
        )?;
        Ok(())
    }

    /// 查询指定主机在 since_ts 之后的指标记录（按时间升序），最多 limit 条。
    pub fn list_metrics(
        &self,
        host_id: &str,
        since_ts: u64,
        limit: u32,
    ) -> rusqlite::Result<Vec<HostMetric>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, host_id, ts, cpu_percent, load1, mem_total_mb, mem_used_mb,
                    mem_percent, disks_json, top_json, source
             FROM host_metrics
             WHERE host_id = ?1 AND ts >= ?2
             ORDER BY ts ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![host_id, since_ts as i64, limit as i64], metric_from_row)?;
        rows.collect()
    }

    /// 删除 before_ts 之前的所有指标记录，返回删除行数。
    pub fn prune_metrics(&self, before_ts: u64) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM host_metrics WHERE ts < ?1",
            params![before_ts as i64],
        )
    }

    /// 统计指定主机的指标记录数（供前端显示数据量）。
    #[allow(dead_code)]
    pub fn metric_count(&self, host_id: &str) -> rusqlite::Result<u64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM host_metrics WHERE host_id = ?1",
            params![host_id],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }
}

fn row_to_host(row: &Row<'_>) -> rusqlite::Result<Host> {
    Ok(Host {
        id: row.get(0)?,
        name: row.get(1)?,
        address: row.get(2)?,
        port: row.get::<_, i64>(3)? as u16,
        username: row.get(4)?,
        auth_type: row.get::<_, String>(5)?.parse().unwrap_or(AuthType::Key),
        key_path: row.get(6)?,
        notes: row.get(7)?,
        created_at: row.get::<_, i64>(8)? as u64,
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

fn row_to_mcp_rule(row: &Row<'_>) -> rusqlite::Result<McpRule> {
    Ok(McpRule {
        id: row.get(0)?,
        pattern: row.get(1)?,
        enabled: row.get::<_, i64>(2)? != 0,
        created_at: row.get::<_, i64>(3)? as u64,
    })
}

fn row_to_terminal_rule(row: &Row<'_>) -> rusqlite::Result<TerminalRule> {
    Ok(TerminalRule {
        id: row.get(0)?,
        pattern: row.get(1)?,
        enabled: row.get::<_, i64>(2)? != 0,
        builtin: row.get::<_, i64>(3)? != 0,
        created_at: row.get::<_, i64>(4)? as u64,
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

fn row_to_inspection_report(row: &Row<'_>) -> rusqlite::Result<InspectionReport> {
    Ok(InspectionReport {
        id: row.get(0)?,
        host_id: row.get(1)?,
        host_label: row.get(2)?,
        provider_id: row.get(3)?,
        provider_name: row.get(4)?,
        model: row.get(5)?,
        status: row.get(6)?,
        risk_level: row.get(7)?,
        summary: row.get(8)?,
        markdown: row.get(9)?,
        html: row.get(10)?,
        email_sent: row.get::<_, i64>(11)? != 0,
        error: row.get(12)?,
        created_at: row.get::<_, i64>(13)? as u64,
        finished_at: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        duration_ms: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
    })
}

fn row_to_remediation(row: &Row<'_>) -> rusqlite::Result<Remediation> {
    let steps_json: String = row.get(9)?;
    let steps = serde_json::from_str(&steps_json).unwrap_or_default();
    Ok(Remediation {
        id: row.get(0)?,
        report_id: row.get(1)?,
        host_id: row.get(2)?,
        host_label: row.get(3)?,
        provider_id: row.get(4)?,
        provider_name: row.get(5)?,
        model: row.get(6)?,
        intervention: row.get(7)?,
        plan_markdown: row.get(8)?,
        steps,
        status: row.get(10)?,
        error: row.get(11)?,
        created_at: row.get::<_, i64>(12)? as u64,
        started_at: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        finished_at: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        duration_ms: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
    })
}

fn metric_from_row(row: &Row<'_>) -> rusqlite::Result<HostMetric> {
    let disks_json: String = row.get(8)?;
    let top_json: String = row.get(9)?;
    let disks: Vec<MetricDisk> =
        serde_json::from_str(&disks_json).unwrap_or_default();
    let top: Vec<MetricTop> = serde_json::from_str(&top_json).unwrap_or_default();
    Ok(HostMetric {
        id: row.get(0)?,
        host_id: row.get(1)?,
        ts: row.get::<_, i64>(2)? as u64,
        cpu_percent: row.get(3)?,
        load1: row.get(4)?,
        mem_total_mb: row.get::<_, i64>(5)? as u64,
        mem_used_mb: row.get::<_, i64>(6)? as u64,
        mem_percent: row.get(7)?,
        disks,
        top,
        source: row.get(10)?,
    })
}
