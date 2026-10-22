//! 大输出归档与本地回查：单个会话私有的临时目录 + 不透明 output:<id> 引用映射，
//! 以及「成功输出 → 预览/归档 → 回查分页」链路上的全部纯函数。
//! 长工具输出完整落盘，模型只见引用与有界预览；需要原文时经 recall_context
//! 按字符区间翻页读取。临时目录的清理不依赖析构：正常退出走显式 cleanup_now
//! （Tauri 的 run() 最终 process::exit，Drop 不可靠），崩溃残留由启动清扫
//! 经 .live 文件锁识别后回收；模型拿到的引用永远不会被还原成文件路径。

use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::context::{message_without_images, split_round_ranges};
use super::context_policy as policy;
use crate::i18n::{is_en, t};

/// 归档目录名前缀：启动清扫按它识别本应用产生的归档目录。
const ARCHIVE_DIR_PREFIX: &str = "buffterm-archive-";
/// 目录存活锁文件名：归档实例持有该文件的进程级锁，
/// 锁随进程退出（含崩溃/强杀）由操作系统释放——锁拿得到即目录已无主。
const LIVE_LOCK_FILE: &str = ".live";
/// 没有锁文件的目录只在早于该时限时才按崩溃残留回收：
/// create() 的「建目录 → 上锁」之间有微小窗口，给并发创建留出让时间。
const STALE_NO_LOCK_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// 会话私有临时目录：在系统临时目录下以随机名创建。
/// Unix 上显式建为 0700（裸 create_dir 受进程 umask 影响，常见的 022 会把
/// 目录暴露给同机其他用户，而归档里是脱敏过但仍敏感的运维证据）、文件 0600；
/// Windows 沿用当前用户私有临时目录的 ACL 继承。目录与文件都按「不存在才建」
/// 语义创建，重名即报错绝不覆盖。
struct PrivateDir {
    /// 目录路径（随机名，归属当前会话归档实例）
    path: PathBuf,
    /// .live 存活锁：目录存续期间保持加锁，供启动清扫区分「活目录」与
    /// 崩溃残留。Option 仅为便于 Drop/cleanup_now 先释放再删目录
    /// （Windows 不允许删除仍被打开的文件所在目录）。
    live: Option<std::fs::File>,
    /// 已经同步清理过：跳过 Drop 里的后台兜底清理
    cleaned: bool,
}

impl PrivateDir {
    /// 惰性创建：首次保存归档时才产生磁盘目录，随后立即建立 .live 存活锁；
    /// 任何一步失败都把半成品目录清掉，不留下无法识别的残留。
    fn create() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "{ARCHIVE_DIR_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        ));
        #[cfg(unix)]
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        #[cfg(not(unix))]
        std::fs::create_dir(&path)?;
        let created = (|| -> std::io::Result<std::fs::File> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let live = options.open(path.join(LIVE_LOCK_FILE))?;
            // 本实例刚建的文件上锁不可能被他人持有，失败按 IO 错误处理
            live.try_lock().map_err(|e| match e {
                std::fs::TryLockError::Error(e) => e,
                std::fs::TryLockError::WouldBlock => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "live lock held")
                }
            })?;
            Ok(live)
        })();
        match created {
            Ok(live) => Ok(Self {
                path,
                live: Some(live),
                cleaned: false,
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&path);
                Err(e)
            }
        }
    }

    /// 先关闭存活锁再删目录：Windows 下打开句柄未关时删除会失败。
    fn release_lock(&mut self) {
        self.live = None;
    }

    /// 显式同步清理：解锁后删除整个目录并标记完成，
    /// 供退出路径与 cleanup_now 调用；不经过 Drop 的后台线程。
    fn cleanup_now(mut self) {
        self.release_lock();
        let _ = std::fs::remove_dir_all(&self.path);
        self.cleaned = true;
    }
}

/// 写入一个归档文件：create_new 保证重名即报错（引用 id 由本归档生成，
/// 冲突意味着外部碰撞/并发异常），Unix 上权限 0600。
fn write_private_file(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())
}

/// 后台清理线程体：尽力删除归档目录，失败静默放弃。
/// 独立成函数便于测试用 JoinHandle 做有界同步等待。
fn cleanup_dir(path: PathBuf) {
    let _ = std::fs::remove_dir_all(path);
}

impl Drop for PrivateDir {
    /// remove_dir_all 在文件较多时可能耗时，挪到后台线程做——归档实例可能在
    /// 会话互斥锁里（reset 换实例）或最后一个 run 的收尾路径上被释放，
    /// 不能在那里阻塞 UI 或全局管理器。这是兜底路径：正常退出依赖
    /// cleanup_now 的显式清理 + 启动清扫兜底，不承诺崩溃级即时清理。
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        self.release_lock();
        let path = self.path.clone();
        let _ = std::thread::Builder::new()
            .name("archive-cleanup".into())
            .spawn(move || cleanup_dir(path));
    }
}

/// 归档内部状态：临时目录惰性创建（首次保存才有磁盘开销），
/// 引用映射与目录同锁，保证「建目录 + 写文件 + 登记」的一致性。
struct ArchiveInner {
    /// 本会话私有的临时目录；None 表示还没有任何归档文件
    dir: Option<PrivateDir>,
    /// output:<id> 引用 → 归档文件路径（路径只对归档内部可见）
    entries: HashMap<String, PathBuf>,
    /// 已写入归档的总字节数：容量上限随会话独立计数，
    /// 长会话不能把临时存储涨成无底洞
    total_bytes: usize,
    /// 本归档的容量上限（字节），默认 policy::ARCHIVE_MAX_BYTES；测试可注入小值
    max_bytes: usize,
    /// 测试用 IO 闸门：挂上后下一次 save 的阻塞任务在收到放行信号前停住，
    /// 用于驱动「IO 未归时取消」的路径。take() 一次性消费。
    #[cfg(test)]
    save_gate: Option<std::sync::mpsc::Receiver<()>>,
    /// 测试用读闸门：与 save_gate 同理，作用于 read 的阻塞任务。
    #[cfg(test)]
    read_gate: Option<std::sync::mpsc::Receiver<()>>,
}

impl Default for ArchiveInner {
    fn default() -> Self {
        Self {
            dir: None,
            entries: HashMap::new(),
            total_bytes: 0,
            max_bytes: policy::ARCHIVE_MAX_BYTES,
            #[cfg(test)]
            save_gate: None,
            #[cfg(test)]
            read_gate: None,
        }
    }
}

/// 保存成功后返回给调用方的引用句柄：随 tool 消息一起进入会话历史。
#[derive(Debug)]
pub(crate) struct OutputReference {
    /// 不透明引用串（output:<uuid>），recall_context 按它回查
    pub reference: String,
    /// 归档正文的 Unicode 字符总数（展示/分页说明用）
    pub total_chars: usize,
}

/// 一次翻页读取的结果：正文与页边界分离，由调用方包装成模型可见文本。
#[derive(Debug)]
pub(crate) struct RecallPage {
    /// 本页正文（[offset, end) 区间的原始字符）
    pub text: String,
    /// 本页起始字符偏移
    pub offset: usize,
    /// 本页结束字符偏移（不含）
    pub end: usize,
    /// 归档正文的 Unicode 字符总数
    pub total: usize,
}

/// 单会话共享的大输出归档：跨命令以 Arc 持有，会话重置时整体替换，
/// 旧归档随仍在收尾的 run 持有的 Arc 释放后由后台线程清理。
/// 归档内容只做原样保存/读取，已脱敏文本才会被送进来。
#[derive(Default)]
pub(crate) struct OutputArchive {
    /// 惰性临时目录与引用映射
    inner: Mutex<ArchiveInner>,
}

/// 引用格式校验：必须是 `output:` + 32 位十六进制（本归档生成引用的唯一形态）。
/// 借此拒绝路径形态（`../`、`/`、`\`）与任何伪造引用——模型输入绝不直接当文件路径用。
fn valid_reference(reference: &str) -> bool {
    let Some(id) = reference.strip_prefix("output:") else {
        return false;
    };
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

impl OutputArchive {
    /// 把完整文本写入本会话归档，返回 output:<id> 引用。
    /// 文件 IO 放在 blocking 线程池，不占用异步执行线程。
    pub(crate) async fn save(self: &Arc<Self>, text: String) -> Result<OutputReference, String> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.save_blocking(&text))
            .await
            .map_err(|e| format!("archive save task failed: {e}"))?
    }

    /// 同步写入实现：惰性创建私有目录，先检查容量再写盘，写盘成功后登记引用。
    fn save_blocking(&self, text: &str) -> Result<OutputReference, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "archive lock poisoned".to_string())?;
        #[cfg(test)]
        if let Some(gate) = inner.save_gate.take() {
            let _ = gate.recv();
        }
        if inner.dir.is_none() {
            inner.dir =
                Some(PrivateDir::create().map_err(|e| format!("cannot create archive dir: {e}"))?);
        }
        let incoming = text.len();
        if inner.total_bytes.saturating_add(incoming) > inner.max_bytes {
            return Err(format!(
                "archive capacity limit reached ({} bytes)",
                inner.max_bytes
            ));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = inner
            .dir
            .as_ref()
            .expect("archive dir just created")
            .path
            .join(format!("out-{id}.txt"));
        write_private_file(&path, text).map_err(|e| format!("cannot write archive file: {e}"))?;
        let reference = format!("output:{id}");
        let total_chars = text.chars().count();
        inner.total_bytes += incoming;
        inner.entries.insert(reference.clone(), path);
        Ok(OutputReference {
            reference,
            total_chars,
        })
    }

    /// 按 Unicode 字符区间翻页读取归档正文。
    /// limit 上限与 recall_context schema 一致（1..=8000）在每个公共入口都校验，
    /// 不依赖调用方把关；offset == total 返回空页，offset > total 报错。
    /// 引用先做格式校验再查登记映射，未知/路径形态引用一律拒绝，不会落到文件系统。
    pub(crate) async fn read(
        self: &Arc<Self>,
        reference: &str,
        offset: usize,
        limit: usize,
    ) -> Result<RecallPage, String> {
        if limit == 0 || limit > policy::RECALL_MAX_LIMIT {
            return Err(format!(
                "recall limit must be within 1..={}",
                policy::RECALL_MAX_LIMIT
            ));
        }
        if !valid_reference(reference) {
            return Err(format!("invalid context reference: {reference}"));
        }
        let path = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "archive lock poisoned".to_string())?;
            inner.entries.get(reference).cloned()
        };
        let Some(path) = path else {
            return Err(format!("unknown context reference: {reference}"));
        };
        #[cfg(test)]
        let gate = self
            .inner
            .lock()
            .map_err(|_| "archive lock poisoned".to_string())?
            .read_gate
            .take();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            Self::read_page(&path, offset, limit)
        })
        .await
        .map_err(|e| format!("archive read task failed: {e}"))?
    }

    /// 显式同步清理本会话归档：进程退出路径调用——Tauri 的正常退出最终
    /// process::exit，不能依赖 Drop 的后台线程收尾。取出私有目录与引用表后
    /// 解锁删除；与并发 save 竞态时，save 看到 dir=None 会惰性重建新目录
    /// （新目录随它自己的生命周期清理/被启动清扫回收）。
    pub(crate) fn cleanup_now(&self) {
        let dir = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            inner.entries.clear();
            inner.total_bytes = 0;
            inner.dir.take()
        };
        if let Some(dir) = dir {
            dir.cleanup_now();
        }
    }

    /// 测试专用：把归档目录指向一个已经存在的文件，使写入确定性地失败，
    /// 用于覆盖「归档 IO 失败 → 完整捕获留在历史」的回退路径。
    /// 返回占位文件路径，由测试负责删除。
    #[cfg(test)]
    pub(crate) fn break_dir_for_test(self: &Arc<Self>) -> PathBuf {
        let blocker = std::env::temp_dir().join(format!(
            "buffterm-blocker-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&blocker, b"x").expect("create blocker file");
        let mut inner = self.inner.lock().expect("archive lock poisoned");
        inner.dir = Some(PrivateDir {
            path: blocker.clone(),
            live: None,
            cleaned: false,
        });
        blocker
    }

    /// 测试专用：注入小容量上限，驱动「归档容量耗尽 → 完整捕获留在历史」。
    #[cfg(test)]
    pub(crate) fn set_max_bytes_for_test(&self, max_bytes: usize) {
        self.inner.lock().expect("archive lock poisoned").max_bytes = max_bytes;
    }

    /// 测试专用：挂起下一次 save 的阻塞任务直到收到放行信号。
    #[cfg(test)]
    pub(crate) fn install_save_gate_for_test(&self, gate: std::sync::mpsc::Receiver<()>) {
        self.inner.lock().expect("archive lock poisoned").save_gate = Some(gate);
    }

    /// 测试专用：挂起下一次 read 的阻塞任务直到收到放行信号。
    #[cfg(test)]
    pub(crate) fn install_read_gate_for_test(&self, gate: std::sync::mpsc::Receiver<()>) {
        self.inner.lock().expect("archive lock poisoned").read_gate = Some(gate);
    }

    /// 测试专用：当前私有目录路径（清理/权限断言用），未创建时为 None。
    #[cfg(test)]
    pub(crate) fn dir_path(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .expect("archive lock poisoned")
            .dir
            .as_ref()
            .map(|dir| dir.path.clone())
    }

    /// 同步翻页实现：按 Unicode 字符（不是字节）切片，
    /// 保证中文/emoji 等非 ASCII 内容不会被截在半个字符上。
    fn read_page(path: &Path, offset: usize, limit: usize) -> Result<RecallPage, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read archive file: {e}"))?;
        let total = text.chars().count();
        if offset > total {
            return Err(format!(
                "offset {offset} is beyond the end of the capture ({total} characters)"
            ));
        }
        let page: String = text.chars().skip(offset).take(limit).collect();
        let end = offset + page.chars().count();
        Ok(RecallPage {
            text: page,
            offset,
            end,
            total,
        })
    }
}

/// 启动清扫：回收上次运行（含崩溃/强杀）残留的归档目录。
/// 识别依据是 .live 存活锁——仍被任一进程持有的目录锁拿不到，直接跳过；
/// 锁拿得到说明持有者已退出，安全删除。没有锁文件的目录只可能是
/// 「建目录 → 上锁」之间崩溃的半成品（或异常删除锁文件），按修改时间
/// 留出让窗口后删除，避免误删并发创建中的目录。
/// 设计为在后台线程运行：临时目录很大时逐个 remove_dir_all 可能耗时。
pub(crate) fn sweep_stale_archives() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(ARCHIVE_DIR_PREFIX) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .open(path.join(LIVE_LOCK_FILE))
        {
            Ok(live) => {
                // 能拿到锁 → 持有者已退出；先关句柄再删（Windows 语义）
                if live.try_lock().is_ok() {
                    drop(live);
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
            Err(_) => {
                let stale = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age > STALE_NO_LOCK_AGE)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }
}

/// 归档失败时随 tool 消息下发的说明：为控制内存，只保留有界头尾摘录。
fn archive_failed_warning() -> String {
    t(
        "无法归档该输出；会话历史中仅保留有界头尾摘录，完整输出已丢弃。",
        "Could not archive this output; only a bounded head/tail excerpt is retained \
         in conversation history and the full output was discarded.",
    )
    .to_string()
}

/// 展示投影：IPC 事件（ai:tool）与历史回显共用的有界文本。
/// 归档失败时 canonical 历史里是完整捕获，但下发前端/回显仍必须有界；
/// 与发送给模型的预览（output_preview）是两条独立路径。
pub(crate) fn display_text(text: &str) -> String {
    crate::util::truncate_output(text, policy::TOOL_DISPLAY_MAX_CHARS)
}

/// recall_context 的分页参数：reference 必填字符串，
/// offset 默认 0、limit 默认 RECALL_DEFAULT_LIMIT 且不超过 RECALL_MAX_LIMIT。
/// reference/offset/limit 之外的键（schema 声明 additionalProperties: false）
/// 与任何类型/越界错误都直接拒绝，不做静默修正；整数先经 usize::try_from
/// 拒绝超出本机指针宽度的值。
pub(crate) fn parse_recall_args(args: &Value) -> Result<(&str, usize, usize), String> {
    if let Some(obj) = args.as_object() {
        for key in obj.keys() {
            if !matches!(key.as_str(), "reference" | "offset" | "limit") {
                return Err(format!("unsupported recall_context argument: {key}"));
            }
        }
    }
    let reference = args
        .get("reference")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            t(
                "缺少 reference 参数",
                "Missing required reference parameter",
            )
            .to_string()
        })?;
    let offset = match args.get("offset") {
        None => 0,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                t(
                    "offset 必须是非负整数",
                    "offset must be a nonnegative integer",
                )
                .to_string()
            })?,
    };
    let limit = match args.get("limit") {
        None => policy::RECALL_DEFAULT_LIMIT,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=policy::RECALL_MAX_LIMIT).contains(n))
            .ok_or_else(|| {
                t(
                    "limit 必须是 1..=8000 的整数",
                    "limit must be an integer in 1..=8000",
                )
                .to_string()
            })?,
    };
    Ok((reference, offset, limit))
}

/// 对一段文本按 Unicode 字符区间取页：offset == total 返回空页，
/// offset > total 报错（与归档分页同一语义）。
pub(crate) fn page_text(
    text: &str,
    offset: usize,
    limit: usize,
) -> Result<(String, usize, usize), String> {
    let total = text.chars().count();
    if offset > total {
        return Err(format!(
            "offset {offset} is beyond the end of the capture ({total} characters)"
        ));
    }
    let page: String = text.chars().skip(offset).take(limit).collect();
    let end = offset + page.chars().count();
    Ok((page, end, total))
}

/// 折叠连续重复行：首行保留，额外重复折成一行计数说明。
/// 只合并「完全相同且相邻」的行，不改动其他任何字节——归档里的完整原文不受影响，
/// 该折叠只作用在发送给模型/前端的预览上。
fn collapse_duplicate_lines(output: &str) -> String {
    let mut result = String::with_capacity(output.len());
    let mut prev: Option<&str> = None;
    let mut dup = 0usize;
    let flush_dup = |result: &mut String, dup: &mut usize| {
        if *dup > 0 {
            // 末行可能不带换行，计数行之前先补一个，避免粘在同一行
            if !result.ends_with('\n') && !result.is_empty() {
                result.push('\n');
            }
            if is_en() {
                result.push_str(&format!("… [same line repeated {dup} more times]\n"));
            } else {
                result.push_str(&format!("… [该行再重复 {dup} 次]\n"));
            }
            *dup = 0;
        }
    };
    for line in output.split_inclusive('\n') {
        if Some(line) == prev {
            dup += 1;
            continue;
        }
        flush_dup(&mut result, &mut dup);
        result.push_str(line);
        prev = Some(line);
    }
    flush_dup(&mut result, &mut dup);
    result
}

/// 长输出的有界预览：先折叠连续重复行，仍超限则按策略头/尾容量截断
/// （尾部保留诊断/退出码/汇总结论）。
fn output_preview(output: &str) -> String {
    let collapsed = collapse_duplicate_lines(output);
    let total = collapsed.chars().count();
    if total <= policy::TOOL_PREVIEW_MAX_CHARS {
        return collapsed;
    }
    let head: String = collapsed
        .chars()
        .take(policy::TOOL_PREVIEW_HEAD_CHARS)
        .collect();
    let tail: String = collapsed
        .chars()
        .skip(total - policy::TOOL_PREVIEW_TAIL_CHARS)
        .collect();
    format!(
        "{head}\n...[{en}]...\n{tail}",
        en = t("输出过长，已截断", "output too long, truncated")
    )
}

/// 成功工具输出写入历史前的处理：超过预览上限的完整捕获存入本会话归档，
/// 历史里只留有界预览 + output:<id> 引用；归档失败时完整原文留在历史
/// （不丢数据）并附说明；未超限原文照存。stored 可能是完整原文，调用方
/// 发往前端/展示时必须再经 display_text 投影。
pub(crate) async fn store_tool_output(archive: &Arc<OutputArchive>, output: String) -> String {
    if output.chars().count() <= policy::TOOL_PREVIEW_MAX_CHARS {
        return output;
    }
    match archive.save(output.clone()).await {
        Ok(OutputReference {
            reference,
            total_chars,
        }) => {
            let preview = output_preview(&output);
            format!(
                "{preview}{}",
                policy::archived_output_notice(&reference, total_chars)
            )
        }
        Err(err) => {
            log::warn!(
                "[agent] {}: {err}",
                t("输出归档失败", "Failed to archive output")
            );
            let excerpt = crate::util::truncate_output(
                &output,
                policy::ARCHIVE_FALLBACK_MAX_CHARS,
            );
            format!("{excerpt}\n{}", archive_failed_warning())
        }
    }
}

/// 把 canonical 历史第 `index` 轮序列化为引用文本：
/// 每条消息 pretty-JSON 逐字输出（call id、工具名、参数、结果全保留），
/// 图片块经 message_without_images 投影替换为占位符——回查的是证据原文，
/// 不是渲染后的摘要，也不复制 base64 数据出来再丢弃。
fn recall_round_text(history: &[Value], index: usize) -> Result<String, String> {
    let (_, ranges) = split_round_ranges(history);
    let Some(range) = ranges.get(index) else {
        return Err(format!("unknown context reference: round:{index}"));
    };
    let mut out = String::new();
    for msg in &history[range.clone()] {
        let msg = message_without_images(msg);
        out.push_str(&serde_json::to_string_pretty(&msg).unwrap_or_default());
        out.push('\n');
    }
    Ok(out)
}

/// 本地上下文回查：`round:<n>` 读 canonical 历史该轮（序列化为引用消息，
/// 图片字节替换为占位符）；`output:<id>` 读当前会话归档。
/// 引用经严格校验（round 下标只允许 ASCII 十进制数字），模型给的串绝不
/// 还原成文件路径。本地只读：不触达审批与远程执行。
pub(crate) async fn recall_context(
    history: &[Value],
    archive: &Arc<OutputArchive>,
    args: &Value,
) -> Result<String, String> {
    let (reference, offset, limit) = parse_recall_args(args)?;
    if let Some(index) = reference.strip_prefix("round:") {
        if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid context reference: {reference}"));
        }
        let index: usize = index
            .parse()
            .map_err(|_| format!("invalid context reference: {reference}"))?;
        let text = recall_round_text(history, index)?;
        let (page, end, total) = page_text(&text, offset, limit)?;
        return Ok(policy::recall_page(reference, offset, end, total, &page));
    }
    if reference.starts_with("output:") {
        let page = archive.read(reference, offset, limit).await?;
        return Ok(policy::recall_page(
            reference,
            page.offset,
            page.end,
            page.total,
            &page.text,
        ));
    }
    Err(format!("invalid context reference: {reference}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn save_and_read_roundtrip_preserves_full_text() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            // 40k 字符，远超旧的 8k/12k 预览上限：归档必须保留完整中段
            let mut text = String::new();
            for i in 0..2000 {
                text.push_str(&format!("line-{i:04} payload\n"));
            }
            let marker = "MIDDLE-DIAGNOSTIC-中文标记-✓";
            text.insert_str(20_000, marker);
            let saved = archive.save(text.clone()).await.unwrap();
            assert!(saved.reference.starts_with("output:"));
            assert_eq!(saved.total_chars, text.chars().count());

            // 命中中段的一页：内容逐字一致
            let page = archive.read(&saved.reference, 19_990, 200).await.unwrap();
            assert_eq!(page.total, saved.total_chars);
            assert_eq!(page.offset, 19_990);
            assert_eq!(
                page.text,
                text.chars().skip(19_990).take(200).collect::<String>()
            );
            assert!(page.text.contains(marker));

            // 翻完整篇逐段拼接必须还原原文
            let mut restored = String::new();
            let mut offset = 0;
            loop {
                let page = archive.read(&saved.reference, offset, 8_000).await.unwrap();
                offset = page.end;
                restored.push_str(&page.text);
                if page.end >= page.total {
                    break;
                }
            }
            assert_eq!(restored, text);
        });
    }

    #[test]
    fn paging_is_by_unicode_chars_not_bytes() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            let text = "日志：磁盘已满 🚨 错误详情".repeat(50);
            let saved = archive.save(text.clone()).await.unwrap();
            let page = archive.read(&saved.reference, 10, 20).await.unwrap();
            let expect: String = text.chars().skip(10).take(20).collect();
            assert_eq!(page.text, expect);
            assert_eq!(page.end, 30);
        });
    }

    #[test]
    fn read_rejects_bad_args_and_references() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            let saved = archive.save("hello".to_string()).await.unwrap();

            for bad_limit in [0usize, policy::RECALL_MAX_LIMIT + 1, usize::MAX] {
                assert!(
                    archive.read(&saved.reference, 0, bad_limit).await.is_err(),
                    "limit={bad_limit} 应拒绝"
                );
            }
            // offset 恰好到末尾 → 空页；超过末尾 → 明确错误
            let page = archive.read(&saved.reference, 5, 10).await.unwrap();
            assert!(page.text.is_empty() && page.end == 5 && page.total == 5);
            assert!(archive.read(&saved.reference, 6, 10).await.is_err());
            // 未知引用（格式合法但未登记）
            assert!(archive
                .read("output:0123456789abcdef0123456789abcdef", 0, 10)
                .await
                .is_err());
            // 路径形态/遍历/前缀错误/长度不符 全部拒绝
            for bad in [
                "output:../etc/passwd",
                "output:..\\..\\win.ini",
                "../output:0123456789abcdef0123456789abcdef",
                "round:0",
                "output:",
                "output:xyz",
                "output:0123456789abcdef0123456789abcdeg",
                "output:0123456789abcdef0123456789abcdef00",
            ] {
                assert!(
                    archive.read(bad, 0, 10).await.is_err(),
                    "引用 {bad:?} 应被拒绝"
                );
            }
        });
    }

    #[test]
    fn archives_are_isolated_per_instance() {
        block_on(async {
            let a = Arc::new(OutputArchive::default());
            let b = Arc::new(OutputArchive::default());
            let saved = a.save("secret text".to_string()).await.unwrap();
            // 别的归档实例（别的会话/主机）不能读这条引用
            let err = b.read(&saved.reference, 0, 10).await.unwrap_err();
            assert!(err.contains("unknown"), "应报未知引用: {err}");
        });
    }

    #[test]
    fn cleanup_runs_in_background_and_removes_dir() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            archive.save("x".repeat(100)).await.unwrap();
            let dir = archive.dir_path().expect("save 后必须有私有目录");
            assert!(dir.exists());
            // 直接驱动清理函数并 join——有界同步，不留悬挂线程
            let cleanup_target = dir.clone();
            let handle = std::thread::spawn(move || cleanup_dir(cleanup_target));
            handle.join().expect("cleanup thread must exit");
            assert!(!dir.exists(), "清理后目录必须消失");
        });
    }

    #[test]
    fn drop_removes_dir_without_blocking() {
        block_on(async {
            let dir = {
                let archive = Arc::new(OutputArchive::default());
                archive.save("x".repeat(100)).await.unwrap();
                archive.dir_path().expect("save 后必须有私有目录")
            };
            // archive 已 drop：后台线程清理目录，轮询等待有界
            for _ in 0..500 {
                if !dir.exists() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("archive dir still exists after drop: {dir:?}");
        });
    }

    /// Unix：目录必须 0700、归档文件必须 0600，不受常见 umask 022 影响。
    #[cfg(unix)]
    #[test]
    fn private_dir_and_files_have_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            archive.save("secret".to_string()).await.unwrap();
            let dir = archive.dir_path().unwrap();
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "目录权限应为 0700, got {dir_mode:o}");
            let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
            let file_mode = file.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "文件权限应为 0600, got {file_mode:o}");
        });
    }

    #[test]
    fn parse_recall_args_validates_strictly() {
        // 默认值
        let args = serde_json::json!({"reference": "round:0"});
        let (r, off, lim) = parse_recall_args(&args).unwrap();
        assert_eq!((r, off, lim), ("round:0", 0, policy::RECALL_DEFAULT_LIMIT));
        // 显式值
        let args = serde_json::json!({"reference": "output:x", "offset": 10, "limit": 100});
        let (r, off, lim) = parse_recall_args(&args).unwrap();
        assert_eq!((r, off, lim), ("output:x", 10, 100));
        // 缺 reference / 空 reference / 非字符串
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"reference": ""}),
            serde_json::json!({"reference": "  "}),
            serde_json::json!({"reference": 5}),
        ] {
            assert!(parse_recall_args(&bad).is_err(), "应拒绝: {bad}");
        }
        // schema 声明 additionalProperties: false —— 多余键必须拒绝
        for bad in [
            serde_json::json!({"reference": "round:0", "path": "/etc/passwd"}),
            serde_json::json!({"reference": "round:0", "extra": 1}),
        ] {
            assert!(parse_recall_args(&bad).is_err(), "应拒绝: {bad}");
        }
        // offset 负数 / 小数 / 字符串（超大值先经 usize::try_from 再交给分页越界检查）
        for bad in [
            serde_json::json!({"reference": "round:0", "offset": -1}),
            serde_json::json!({"reference": "round:0", "offset": 1.5}),
            serde_json::json!({"reference": "round:0", "offset": "x"}),
        ] {
            assert!(parse_recall_args(&bad).is_err(), "应拒绝: {bad}");
        }
        // limit 越界 / 非整数
        for bad in [
            serde_json::json!({"reference": "round:0", "limit": 0}),
            serde_json::json!({"reference": "round:0", "limit": 8001}),
            serde_json::json!({"reference": "round:0", "limit": -3}),
        ] {
            assert!(parse_recall_args(&bad).is_err(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn page_text_pages_by_unicode_chars() {
        let text = "中文abc🚨x".repeat(20);
        let (page, end, total) = page_text(&text, 5, 10).unwrap();
        assert_eq!(page, text.chars().skip(5).take(10).collect::<String>());
        assert_eq!(end, 15);
        assert_eq!(total, text.chars().count());
        // offset 到末尾 → 空页；超出末尾 → 错
        let (page, end, _) = page_text(&text, total, 10).unwrap();
        assert!(page.is_empty() && end == total);
        assert!(page_text(&text, total + 1, 10).is_err());
    }

    #[test]
    fn collapse_duplicate_lines_only_merges_consecutive_identical() {
        let text = "a\nsame\nsame\nsame\nb\nsame\n";
        let out = collapse_duplicate_lines(text);
        // "same\n" 连续 3 次：保留 1 行 + 计数行；后一个孤立的 "same" 不动
        assert_eq!(
            out.matches("same\n").count() + out.matches("same").count() - 2,
            2,
            "{out}"
        );
        assert!(out.contains("a\n") && out.contains("b\n"), "{out}");
        assert!(
            out.contains("重复 2 次") || out.contains("repeated 2"),
            "{out}"
        );
        // 无重复时原样返回
        let plain = "x\ny\nz";
        assert_eq!(collapse_duplicate_lines(plain), plain);
    }

    #[test]
    fn output_preview_bounds_long_output() {
        let output = format!("{}\n{}", "H".repeat(6_000), "T".repeat(6_000));
        let preview = output_preview(&output);
        assert!(
            preview.chars().count() <= policy::TOOL_PREVIEW_MAX_CHARS,
            "预览必须有界"
        );
        assert!(preview.starts_with("HHHH"), "保留头部: {}", &preview[..20]);
        assert!(preview.ends_with("TTTT"), "保留尾部");
        // 短输出原样
        assert_eq!(output_preview("short"), "short");
        // 重复行折叠后可以缩短到预览上限内且不截断
        let spam = "progress...\n".repeat(2_000);
        let collapsed = output_preview(&spam);
        assert!(collapsed.chars().count() < spam.chars().count());
        assert!(
            !collapsed.contains("已截断"),
            "折叠后未超限不应截断: {}",
            collapsed.len()
        );
    }

    #[test]
    fn display_text_bounds_ipc_output() {
        // 归档失败时 canonical 里是多 MB 完整捕获；展示投影必须有界并带显式标记
        let giant = "D".repeat(50_000);
        let shown = display_text(&giant);
        assert!(
            shown.chars().count() <= policy::TOOL_DISPLAY_MAX_CHARS + 80,
            "展示文本必须有界（上限 + 标记开销）: {}",
            shown.chars().count()
        );
        assert!(
            shown.contains("已省略") || shown.contains("omitted"),
            "必须带显式截断标记: {}",
            &shown[shown.len() / 2..shown.len() / 2 + 60]
        );
        // 未超限原文照传（标记不会出现）
        assert_eq!(display_text("short output"), "short output");
    }

    #[test]
    fn recall_round_text_serializes_and_strips_images() {
        let history = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,BIGDATA"}},
            ]}),
            serde_json::json!({"role": "assistant", "content": "done", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "exec_command", "arguments": "{\"command\":\"ls\"}"}}
            ]}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "out"}),
            serde_json::json!({"role": "user", "content": "第二轮"}),
        ];
        let text = recall_round_text(&history, 0).unwrap();
        assert!(text.contains("看图"), "应保留文本部分: {text}");
        assert!(!text.contains("BIGDATA"), "图片 base64 必须被剥离");
        assert!(
            text.contains("[图片]") || text.contains("[image]"),
            "应有图片占位符"
        );
        assert!(
            text.contains("exec_command") && text.contains("c1"),
            "tool_calls 逐字保留"
        );
        // 越界索引 / 未知引用
        assert!(recall_round_text(&history, 99).is_err());
    }

    #[test]
    fn recall_context_round_and_output_and_rejects() {
        block_on(async {
            let history = vec![
                serde_json::json!({"role": "system", "content": "sys"}),
                serde_json::json!({"role": "user", "content": "第一句"}),
                serde_json::json!({"role": "assistant", "content": "回答一"}),
                serde_json::json!({"role": "user", "content": "第二句"}),
                serde_json::json!({"role": "assistant", "content": "回答二"}),
            ];
            let archive = Arc::new(OutputArchive::default());
            let saved = archive.save("ABCDEFGHIJ".repeat(500)).await.unwrap();

            // round 回查：quoted data 包装 + 原文内容
            let out = recall_context(
                &history,
                &archive,
                &serde_json::json!({"reference": "round:0"}),
            )
            .await
            .unwrap();
            assert!(out.contains("第一句") && out.contains("回答一"), "{out}");
            assert!(!out.contains("第二句"), "round:0 不应包含第二轮: {out}");
            assert!(out.contains("[Recalled context") && out.contains("[End recalled context]"));

            // output 回查 + 翻页恢复中段
            let out = recall_context(
                &history,
                &archive,
                &serde_json::json!({
                    "reference": saved.reference, "offset": 100, "limit": 50
                }),
            )
            .await
            .unwrap();
            assert!(
                out.contains("characters=100..150") && out.contains("total=5000"),
                "{out}"
            );

            // 未知引用 / 遍历形态 / 非法前缀 / 非十进制下标
            for bad in [
                serde_json::json!({"reference": "round:99"}),
                serde_json::json!({"reference": "round:-1"}),
                serde_json::json!({"reference": "round:+1"}),
                serde_json::json!({"reference": "round:"}),
                serde_json::json!({"reference": "round:1a"}),
                serde_json::json!({"reference": "../etc/passwd"}),
                serde_json::json!({"reference": "output:../x"}),
                serde_json::json!({"reference": "file:///etc/passwd"}),
                serde_json::json!({"reference": "output:0123456789abcdef0123456789abcdef"}),
            ] {
                assert!(
                    recall_context(&history, &archive, &bad).await.is_err(),
                    "应拒绝: {bad}"
                );
            }
        });
    }

    #[test]
    fn store_tool_output_archives_and_falls_back_on_io_failure() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            // 短输出：原文照存
            assert_eq!(store_tool_output(&archive, "ok".into()).await, "ok");

            // 长输出：预览有界 + 归档引用可回查中段
            let middle_marker = "MID-DIAG-中文";
            let mut output = "H".repeat(6_000);
            output.push_str(middle_marker);
            output.push_str(&"T".repeat(6_000));
            let stored = store_tool_output(&archive, output.clone()).await;
            assert!(
                stored.chars().count() < output.chars().count(),
                "写回历史的是预览而非全文"
            );
            let reference = stored
                .split("available as ")
                .nth(1)
                .and_then(|s| s.split('.').next())
                .expect("应包含 output:<id> 引用");
            let page = archive.read(reference, 6_000, 100).await.unwrap();
            assert!(
                page.text.contains(middle_marker),
                "归档必须保留旧预览上限之外的中段: {}",
                page.text
            );

            // 归档 IO 失败：只保留有界头尾摘录，避免长会话把完整 8 MB 输出
            // 永久留在 canonical history 里；中段诊断应被省略。
            let broken = Arc::new(OutputArchive::default());
            let blocker = broken.break_dir_for_test();
            let mut large_output = "H".repeat(50_000);
            large_output.push_str(middle_marker);
            large_output.push_str(&"T".repeat(50_000));
            let stored = store_tool_output(&broken, large_output.clone()).await;
            assert!(
                stored.chars().count() <= policy::ARCHIVE_FALLBACK_MAX_CHARS + 200,
                "归档失败回退必须保持有界: {}",
                stored.chars().count()
            );
            assert!(!stored.contains(&large_output), "不应保留完整大输出");
            assert!(!stored.contains(middle_marker), "中段应被省略");
            assert!(
                stored.contains("bounded head/tail excerpt")
                    || stored.contains("有界头尾摘录"),
                "应附归档失败说明: {}",
                &stored[stored.len() - 300..]
            );
            let shown = display_text(&stored);
            assert!(
                shown.chars().count() <= policy::TOOL_DISPLAY_MAX_CHARS + 80,
                "事件/回显投影必须有界: {}",
                shown.chars().count()
            );
            let _ = std::fs::remove_file(blocker);
        });
    }

    /// 显式清理：目录同步删除、旧引用立即失效；之后的保存惰性重建新目录（自愈），
    /// 不依赖 Drop——退出路径上的清理不能指望 Tauri process::exit 前跑析构。
    #[test]
    fn cleanup_now_removes_dir_and_invalidates_references() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            let saved = archive.save("x".repeat(100)).await.unwrap();
            let dir = archive.dir_path().expect("save 后必须有私有目录");
            assert!(dir.exists());
            archive.cleanup_now();
            assert!(!dir.exists(), "显式清理后目录必须消失");
            assert!(
                archive.read(&saved.reference, 0, 10).await.is_err(),
                "旧引用在清理后必须失效"
            );
            let saved2 = archive.save("y".repeat(50)).await.unwrap();
            let page = archive.read(&saved2.reference, 0, 10).await.unwrap();
            assert_eq!(page.text, "yyyyyyyyyy", "清理后的归档必须可正常读写");
        });
    }

    /// 容量上限：超过配额的保存被拒绝，store_tool_output 退回「有界头尾摘录
    /// + 说明」的口径，临时存储与 canonical history 都不会随长会话无限增长。
    #[test]
    fn capacity_limit_falls_back_to_bounded_excerpt() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            archive.set_max_bytes_for_test(4_096);
            archive.save("z".repeat(3_000)).await.unwrap();
            let err = archive.save("z".repeat(3_000)).await.unwrap_err();
            assert!(err.contains("capacity"), "应为容量拒绝: {err}");

            let long = "w".repeat(100_000);
            let stored = store_tool_output(&archive, long.clone()).await;
            assert!(
                stored.chars().count() <= policy::ARCHIVE_FALLBACK_MAX_CHARS + 200,
                "容量拒绝回退必须保持有界: {}",
                stored.chars().count()
            );
            assert!(!stored.contains(&long), "不应保留完整大输出");
            assert!(
                stored.contains("bounded head/tail excerpt")
                    || stored.contains("有界头尾摘录"),
                "应附归档失败说明: {}",
                &stored[stored.len() - 300..]
            );
        });
    }

    /// 存活锁：归档目录存续期间 .live 文件保持加锁，
    /// 启动清扫据此识别「仍被持有」的目录。
    #[test]
    fn live_lock_file_is_held_while_dir_exists() {
        block_on(async {
            let archive = Arc::new(OutputArchive::default());
            archive.save("x".repeat(10)).await.unwrap();
            let dir = archive.dir_path().unwrap();
            let lock_path = dir.join(LIVE_LOCK_FILE);
            assert!(lock_path.exists(), "归档目录内必须有 .live 锁文件");
            let other = std::fs::OpenOptions::new()
                .write(true)
                .open(&lock_path)
                .unwrap();
            assert!(other.try_lock().is_err(), "存活中的归档锁必须拿不到");
        });
    }

    /// 启动清扫：锁已释放的残留目录被回收；仍被持有的目录与
    /// 「建目录 → 上锁」窗口内的新目录都不得误删。
    #[test]
    fn sweep_removes_dead_dirs_and_skips_locked() {
        block_on(async {
            let alive = Arc::new(OutputArchive::default());
            alive.save("live".to_string()).await.unwrap();
            let alive_dir = alive.dir_path().unwrap();

            // 死目录：有 .live 但锁已释放（持有者退出）
            let dead = std::env::temp_dir().join(format!(
                "{ARCHIVE_DIR_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&dead).unwrap();
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dead.join(LIVE_LOCK_FILE))
                .unwrap();
            lock.try_lock().expect("新目录的锁必须拿得到");
            drop(lock); // 释放 = 持有者已退出

            // 无锁新目录：处于创建窗口，清扫必须跳过
            let fresh = std::env::temp_dir().join(format!(
                "{ARCHIVE_DIR_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&fresh).unwrap();

            sweep_stale_archives();

            assert!(alive_dir.exists(), "存活中的归档不得被清扫");
            assert!(!dead.exists(), "锁已释放的残留目录必须回收");
            assert!(fresh.exists(), "无锁的新目录不得误删");
            let _ = std::fs::remove_dir_all(&fresh);
        });
    }
}
