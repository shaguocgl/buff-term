//! 后端国际化（i18n）。
//!
//! 界面语言由前端通过 `set_app_language` 同步，持久化在 settings 表（键 `app_language`）。
//! 后端所有对用户可见的文案（错误提示、AI 系统提示词、工具描述、邮件内容）都按当前语言生成：
//! 简单短语用 [`t`] 就地二选一（中/英），多行提示词用 [`is_en`] 分支书写。
//!
//! 默认语言为中文：既保持既有行为，也让 `cargo test` 中的中文断言（未显式切换语言时）继续成立。

use crate::db::Db;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tauri::State;

/// 界面语言
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    /// 简体中文
    Zh,
    /// English
    En,
}

/// 当前语言（0 = 中文，1 = 英文）。用原子变量保存，读取无需加锁。
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 当前语言
pub fn lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 1 {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// 是否为英文界面（多行提示词分支判断用）
pub fn is_en() -> bool {
    lang() == Lang::En
}

/// 切换当前语言（仅改内存状态，持久化见 `set_app_language`）
pub fn set_lang(next: Lang) {
    CURRENT.store(
        match next {
            Lang::Zh => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// 解析语言标识：`en` → 英文，其余一律中文
pub fn parse_lang(value: &str) -> Lang {
    if value.eq_ignore_ascii_case("en") {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// 语言标识（与前端 `Lang` 一致）
pub fn as_str(value: Lang) -> &'static str {
    match value {
        Lang::Zh => "zh",
        Lang::En => "en",
    }
}

/// 按当前语言二选一（中 / 英）
pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 应用启动时载入已保存的语言（键不存在 / 损坏时保持默认中文）
pub fn init(db: &Db) {
    if let Ok(Some(saved)) = db.get_setting("app_language") {
        set_lang(parse_lang(&saved));
    }
}

/// 读取当前界面语言
#[tauri::command]
pub fn get_app_language() -> String {
    as_str(lang()).to_string()
}

/// 切换界面语言并持久化：后续 AI 提示词、工具描述与错误文案都按新语言生成
#[tauri::command]
pub fn set_app_language(db: State<'_, Arc<Db>>, lang: String) -> Result<(), String> {
    let next = parse_lang(&lang);
    set_lang(next);
    db.set_setting("app_language", as_str(next))
        .map_err(|e| format!("{}: {e}", t("保存语言设置失败", "Failed to save language setting")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_language_codes() {
        assert_eq!(parse_lang("en"), Lang::En);
        assert_eq!(parse_lang("EN"), Lang::En);
        assert_eq!(parse_lang("zh"), Lang::Zh);
        assert_eq!(parse_lang(""), Lang::Zh);
        assert_eq!(parse_lang("fr"), Lang::Zh);
    }

    #[test]
    fn language_codes_round_trip() {
        assert_eq!(as_str(Lang::Zh), "zh");
        assert_eq!(as_str(Lang::En), "en");
    }

    #[test]
    fn t_picks_text_for_current_language() {
        // 默认语言为中文；此处不切换全局状态，避免与并行运行的其它测试互相干扰
        assert_eq!(lang(), Lang::Zh);
        assert_eq!(t("中文", "English"), "中文");
    }
}
