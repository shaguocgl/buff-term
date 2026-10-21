//! 终端会话远端工作目录（cwd）跟踪：独立于终端守护开关——在会话输入的
//! 串行消费者里逐字节重建当前编辑行，回车提交时优先采用前端读取的
//! console_line（xterm 光标行，含补全/历史回放后的最终命令），从中识别
//! cd / pushd / popd 并更新会话 cwd；遇到 ssh / su / sudo -i 等会切换
//! 执行上下文的命令时置为未知（SFTP 面板跟随一个错误目录比不跟随更糟）。
//! 本地行只做 console_line 缺失时的兜底，不做 readline 编辑追踪——
//! 跟踪失败宁可报「未知」也不猜。

/// 会话级目录状态：cwd 之外额外记录 oldpwd（cd -）与 pushd 目录栈
#[derive(Debug, Default)]
pub(crate) struct CwdState {
    /// 当前远端工作目录；None = 未知（尚未探测/上下文已切换/无法解析）
    pub cwd: Option<String>,
    /// 上一个目录：cd - 与 pushd/popd 的语义需要
    oldpwd: Option<String>,
    /// 远端 $HOME（会话建立后探测一次）：cd 无参 / ~/ 展开 / 相对路径兜底
    home: Option<String>,
    /// pushd 目录栈（栈底在前）
    stack: Vec<String>,
    /// shell 嵌套栈：bash/zsh 等子 shell 启动时记录进入点目录与 pushd 栈，
    /// exit/logout 弹栈恢复——子 shell 里的 cd 不作用于父 shell
    shells: Vec<ShellFrame>,
}

/// 一层 shell 嵌套的记忆：进入子 shell 时父 shell 的 cwd/oldpwd/pushd 栈
#[derive(Debug)]
struct ShellFrame {
    cwd: Option<String>,
    oldpwd: Option<String>,
    stack: Vec<String>,
}

impl CwdState {
    /// 会话建立后注入远端 home；cwd 未初始化时以 home 起步
    /// （登录 shell 的初始目录即 home）。
    pub fn set_home(&mut self, home: String) {
        if self.cwd.is_none() {
            self.oldpwd = Some(home.clone());
            self.cwd = Some(home.clone());
        }
        self.home = Some(home);
    }
}

/// 逐字节跟踪当前编辑行的轻量状态机。
/// 不重建 readline 的全部编辑语义（方向键/历史/补全后本地行会失真）——
/// 那些场景由前端 console_line 兜底；本地行的职责是：
/// 无 console_line 时提供候选命令 + 给 console_line 提取结果做一致性校验。
/// ESC 序列消费状态：方向键/功能键/鼠标上报都不应进本地行。
/// CSI（ESC [ …终字节）与 SS3（ESC O +终字节）覆盖方向键与功能键；
/// OSC（ESC ] …BEL/ST）覆盖标题/剪贴板序列；其余 ESC+x 视为两字节序列。
#[derive(Default, PartialEq)]
enum Esc {
    #[default]
    None,
    /// 刚收到 ESC，等下一个字节判定序列形态
    Start,
    /// CSI/SS3：参数/中间字节（0x20-0x3f）继续，0x40-0x7e 终结
    Csi,
    /// OSC：BEL 或 ESC \ 终结（ESC 起头按新序列重新判定）
    Osc,
}

#[derive(Default)]
pub(crate) struct CwdTracker {
    /// 当前行原始字节（UTF-8 序列按字节累积，提交时 lossy 解码）
    line: Vec<u8>,
    /// 正在消费的 ESC 序列；方向键/编辑键经过后本地行可信度下降，
    /// 仅作 console_line 的一致性参照
    esc: Esc,
}

impl CwdTracker {
    /// 透传模式（alternate screen 全屏应用）下调用：应用内按键不是 shell 命令，
    /// 丢弃本地行，避免 vim 里的 "cd" 文本被误当成目录切换。
    pub fn reset(&mut self) {
        self.line.clear();
        self.esc = Esc::None;
    }

    /// 喂入一段终端输入。返回所有提交的命令行（粘贴可能一次提交多行）。
    /// console_line 与输入同路到达（前端回车时读取），仅在含回车的输入上有值。
    pub fn on_input(&mut self, data: &[u8], console_line: Option<&str>) -> Vec<String> {
        let mut submitted = Vec::new();
        for &b in data {
            match self.esc {
                Esc::Start => {
                    self.esc = match b {
                        b'[' | b'O' => Esc::Csi,
                        b']' => Esc::Osc,
                        0x1b => Esc::Start,
                        _ => Esc::None, // 两字节 ESC 序列（如 ESC c）
                    };
                    continue;
                }
                Esc::Csi => {
                    self.esc = match b {
                        0x1b => Esc::Start,
                        0x40..=0x7e => Esc::None,
                        _ => Esc::Csi,
                    };
                    continue;
                }
                Esc::Osc => {
                    self.esc = match b {
                        0x07 => Esc::None,
                        0x1b => Esc::Start,
                        _ => Esc::Osc,
                    };
                    continue;
                }
                Esc::None => {}
            }
            match b {
                0x1b => self.esc = Esc::Start,
                0x0d | 0x0a => {
                    if let Some(cmd) = self.submit_line(console_line) {
                        submitted.push(cmd);
                    }
                }
                // 退格：先弹尾随的 UTF-8 续字节，再弹引导字节，
                // 避免把一个多字节字符删成半残字节
                0x08 | 0x7f => {
                    while (self.line.last().copied().unwrap_or(0) & 0xc0) == 0x80 {
                        self.line.pop();
                    }
                    self.line.pop();
                }
                // Ctrl-C / Ctrl-D / Ctrl-U：行被丢弃或清除
                0x03 | 0x04 | 0x15 => self.line.clear(),
                // 其余 C0 控制字符（Tab、Ctrl-W 等）不改变「已输入内容」的乐观记录；
                // 0x80+ 字节是 UTF-8 引导/续字节，必须进 line
                0x00..=0x1f => {}
                _ => self.line.push(b),
            }
        }
        submitted
    }

    /// 回车提交：优先前端 console_line（剥提示符 + 与本地行一致性校验），
    /// 否则用本地行。返回规范化后的命令文本；空行返回 None。
    fn submit_line(&mut self, console_line: Option<&str>) -> Option<String> {
        let local = std::mem::take(&mut self.line);
        let local_text = String::from_utf8_lossy(&local).trim().to_string();
        let cmd = console_line
            .map(crate::guard::extract_command_from_console_line)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|c| crate::guard::console_line_consistent(c, &local))
            .unwrap_or(local_text);
        if cmd.is_empty() { None } else { Some(cmd) }
    }
}

/// 解析一条已提交命令对 cwd 的影响：按 ; | && || 切分段落，逐段应用。
/// 任何无法解析的目录变更 → Unknown（宁可不知道也不指错目录）。
fn cwd_op_of_command(cmd: &str, state: &mut CwdState) {
    // 整体含未配对括号的段落是子 shell / 分组语义，直接放弃解析
    let mut depth: i32 = 0;
    for c in cmd.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            state.cwd = None;
            return;
        }
    }
    if depth != 0 {
        state.cwd = None;
        return;
    }
    for segment in split_segments(cmd) {
        // 含 | / & 的段是管道或后台化：bash 里管道每个阶段、& 前缀的命令都跑在
        // 子 shell，其中的 cd 不改变父 shell 目录——整段跳过不做判定
        if segment.contains('|') || segment.contains('&') {
            continue;
        }
        let argv = match shell_words(&segment) {
            Some(a) => a,
            // 分词失败（引号不配对、含 $VAR/通配符等）：若段落以目录/上下文
            // 命令开头，语义不可知 → 标记未知；否则与目录无关，跳过
            None => {
                if segment_is_relevant(&segment) {
                    state.cwd = None;
                    return;
                }
                continue;
            }
        };
        if argv.is_empty() {
            continue;
        }
        match argv[0].as_str() {
            "cd" | "pushd" => {
                let push = argv[0] == "pushd";
                // 跳过 cd -L/-P/-- 选项位，取第一个非选项参数作目标
                let target = argv[1..]
                    .iter()
                    .map(|s| s.as_str())
                    .find(|a| !matches!(*a, "-L" | "-P" | "-LP" | "-PL"));
                match resolve_dir(target, &state) {
                    Some(dir) => {
                        let old = state.cwd.clone();
                        if push {
                            if let Some(o) = old.clone() {
                                state.stack.push(o);
                            }
                        }
                        state.oldpwd = old;
                        state.cwd = Some(dir);
                    }
                    None => {
                        state.cwd = None;
                        return;
                    }
                }
            }
            "popd" => {
                // 栈空时 popd 不改变目录；解析不了就记未知
                match state.stack.pop() {
                    Some(dir) => {
                        state.oldpwd = state.cwd.clone();
                        state.cwd = Some(dir);
                    }
                    None => {
                        state.cwd = None;
                        return;
                    }
                }
            }
            // 切换到别的执行上下文：远端 cwd 不再是本机路径
            "ssh" | "telnet" | "mosh" | "su" | "doas" => {
                state.cwd = None;
                return;
            }
            // 子 shell：压栈记录进入点；exit/logout 弹栈恢复父 shell 目录。
            // cd 在子 shell 里的跟踪照常进行，退出时整体丢弃
            "bash" | "sh" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh" => {
                // bash -c 'cmd' 之类不进入交互子 shell，exec 后直接返回；
                // 其中含 cd 也不影响父 shell——压栈后永不弹栈反而丢跟踪，
                // 所以带 -c/-s 参数的形态直接跳过
                if !argv[1..]
                    .iter()
                    .any(|a| a.starts_with("-c") || a.starts_with("-s") || a == "-")
                {
                    state.shells.push(ShellFrame {
                        cwd: state.cwd.clone(),
                        oldpwd: state.oldpwd.clone(),
                        stack: std::mem::take(&mut state.stack),
                    });
                }
            }
            // exit/logout：有嵌套时弹栈恢复父 shell；顶层 shell 退出即会话结束，
            // 目录无从谈起，同样记未知
            "exit" | "logout" => match state.shells.pop() {
                Some(frame) => {
                    state.cwd = frame.cwd;
                    state.oldpwd = frame.oldpwd;
                    state.stack = frame.stack;
                }
                None => {
                    state.cwd = None;
                    return;
                }
            },
            // exec 用新程序替换当前 shell（不会再返回），语义不可知
            "exec" => {
                state.cwd = None;
                return;
            }
            // sudo -i / sudo su / sudo bash 等换 root 登录环境
            "sudo" => {
                if argv[1..]
                    .iter()
                    .any(|a| matches!(a.as_str(), "-i" | "--login" | "su" | "bash" | "sh" | "zsh" | "fish"))
                {
                    state.cwd = None;
                    return;
                }
            }
            _ => {}
        }
    }
}

/// 把已提交命令行应用到会话 cwd 状态。
pub(crate) fn apply_command(cmd: &str, state: &mut CwdState) {
    cwd_op_of_command(cmd, state);
}

/// 按顺序执行的语句分隔符切分：; && || ——忽略引号内的分隔符。
/// 刻意不按 | / & 切分：管道各阶段与后台命令在子 shell 执行，其中的 cd
/// 不改变父 shell 目录；含它们的段由调用方整段跳过。
fn split_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c2) if c2 == q => quote = None,
            // 反斜杠只在双引号内是转义；单引号内是字面量（bash 规则）
            (Some('"'), '\\') => {
                cur.push(c);
                if let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                }
                continue;
            }
            (None, '\'' | '"') => quote = Some(c),
            (None, ';') => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            (None, '&' | '|') => {
                if chars.peek() == Some(&c) {
                    chars.next();
                    out.push(std::mem::take(&mut cur));
                    continue;
                }
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out
}

/// 分词失败的段落是否可能含目录/上下文命令：词首是 cd/pushd/popd/ssh 等
/// 才需要按「不可解析」处理（避免 `echo "未闭合引号` 把 cwd 误标未知）。
fn segment_is_relevant(segment: &str) -> bool {
    let first = segment
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .next()
        .unwrap_or("");
    matches!(
        first,
        "cd" | "pushd" | "popd" | "ssh" | "telnet" | "mosh" | "su" | "doas" | "sudo"
            | "bash" | "sh" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh"
            | "exit" | "logout" | "exec"
    )
}

/// 极简 shell 分词：单/双引号、反斜杠转义；~ 与 $HOME/${HOME} 在词首展开，
/// 其余 $VAR / $() / 通配符一律使整条命令标记为不可解析（返回 None）。
fn shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = segment.trim().chars().peekable();
    let mut has_char = false;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), c2) => cur.push(c2),
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') => {
                if let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                }
            }
            (Some('"'), '$') => return None,
            // quote 只会是 ' 或 "；通配兜底满足穷尽性
            (Some(_), c2) => cur.push(c2),
            (None, '\'' | '"') => {
                quote = Some(c);
                has_char = true;
            }
            (None, '\\') => {
                if let Some(&n) = chars.peek() {
                    cur.push(n);
                    chars.next();
                }
            }
            (None, c2) if c2.is_whitespace() => {
                if has_char || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    has_char = false;
                }
            }
            (None, '$' | '*' | '?' | '[' | '`') => return None,
            (None, '(' | ')') => return None,
            (None, c2) => {
                cur.push(c2);
                has_char = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_char || !cur.is_empty() {
        words.push(cur);
    }
    Some(words)
}

/// cd 目标 → 绝对路径：无参/~→home，-→oldpwd，绝对路径规范化，
/// ~x/$HOME 前缀展开已在 shell_words 拒绝 $；相对路径并入当前 cwd。
fn resolve_dir(arg: Option<&str>, state: &CwdState) -> Option<String> {
    match arg {
        None | Some("~") | Some("--") => state.home.clone(),
        Some("-") => state.oldpwd.clone(),
        Some(raw) => {
            let expanded = if let Some(rest) = raw.strip_prefix('~') {
                if rest.is_empty() {
                    state.home.clone()?
                } else if let Some(r) = rest.strip_prefix('/') {
                    join_normalize(state.home.as_deref().unwrap_or("/"), r)
                } else {
                    // ~user 展开不做（不知道其他用户目录）
                    return None;
                }
            } else {
                raw.to_string()
            };
            if expanded.starts_with('/') {
                Some(normalize(&expanded))
            } else {
                let base = state.cwd.clone()?;
                Some(join_normalize(&base, &expanded))
            }
        }
    }
}

/// 规范化绝对路径：处理 //、/./、/../，始终以 / 开头。
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn join_normalize(base: &str, rel: &str) -> String {
    let joined = if base.ends_with('/') {
        format!("{base}{rel}")
    } else {
        format!("{base}/{rel}")
    };
    normalize(&joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_at(dir: &str) -> CwdState {
        CwdState {
            cwd: Some(dir.to_string()),
            oldpwd: None,
            home: Some("/root".to_string()),
            stack: Vec::new(),
            shells: Vec::new(),
        }
    }

    #[test]
    fn cd_absolute_and_relative() {
        let mut s = state_at("/root");
        apply_command("cd /etc", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/etc"));
        apply_command("cd nginx", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/etc/nginx"));
        apply_command("cd ..", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/etc"));
        apply_command("cd ../../var//log/./", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/var/log"));
    }

    #[test]
    fn cd_home_dash_and_oldpwd() {
        let mut s = state_at("/var");
        apply_command("cd", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"));
        apply_command("cd /tmp", &mut s);
        apply_command("cd -", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"));
        apply_command("cd -", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn pushd_popd_stack() {
        let mut s = state_at("/root");
        apply_command("pushd /etc", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/etc"));
        apply_command("pushd /var", &mut s);
        apply_command("popd", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/etc"));
    }

    #[test]
    fn compound_and_quotes() {
        let mut s = state_at("/root");
        apply_command("ls && cd /opt && pwd", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/opt"));
        apply_command("cd \"a b\"", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/opt/a b"));
        apply_command("cd 'x y'", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/opt/a b/x y"));
        apply_command("echo 'a;b' ; cd /tmp", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn unresolvable_marks_unknown() {
        let mut s = state_at("/root");
        apply_command("cd $FOO", &mut s);
        assert!(s.cwd.is_none());
        let mut s = state_at("/root");
        apply_command("cd $(pwd)", &mut s);
        assert!(s.cwd.is_none());
        let mut s = state_at("/root");
        apply_command("cd ~other", &mut s);
        assert!(s.cwd.is_none());
    }

    #[test]
    fn context_switching_commands_reset() {
        for cmd in ["ssh dev@prod", "su -", "sudo -i", "sudo su", "telnet h"] {
            let mut s = state_at("/root");
            apply_command(cmd, &mut s);
            assert!(s.cwd.is_none(), "{cmd} 应把 cwd 置为未知");
        }
        // sudo 的普通命令不换上下文
        let mut s = state_at("/root");
        apply_command("sudo systemctl status sshd", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"));
    }

    #[test]
    fn home_expansion() {
        let mut s = state_at("/var");
        apply_command("cd ~/work", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root/work"));
    }

    #[test]
    fn tracker_reconstructs_simple_line() {
        let mut tr = CwdTracker::default();
        assert!(tr.on_input(b"cd /et", None).is_empty());
        let cmds = tr.on_input(b"c\r", None);
        assert_eq!(cmds, vec!["cd /etc"]);
    }

    #[test]
    fn tracker_prefers_console_line() {
        let mut tr = CwdTracker::default();
        tr.on_input(b"cd /etc/ng", None);
        // Tab 补全后本地行只保留了前缀；console_line 含补全后的完整命令
        tr.on_input(&[0x09], None); // Tab
        let cmds = tr.on_input(b"\r", Some("root@h:~# cd /etc/nginx"));
        assert_eq!(cmds, vec!["cd /etc/nginx"]);
    }

    #[test]
    fn tracker_esc_sequences_do_not_pollute_line() {
        let mut tr = CwdTracker::default();
        tr.on_input(b"cd /e", None);
        // ↑（历史回放）、←→、F1、OSC 标题设置都不应进入本地行
        tr.on_input(&[0x1b, 0x5b, 0x41], None); // ESC [ A
        tr.on_input(&[0x1b, 0x5b, 0x44], None); // ESC [ D
        tr.on_input(&[0x1b, 0x4f, 0x50], None); // ESC O P
        tr.on_input(b"\x1b]0;title\x07", None); // OSC ... BEL
        let cmds = tr.on_input(b"\r", None);
        assert_eq!(cmds, vec!["cd /e"], "ESC 序列字节不得进入命令行");
    }

    #[test]
    fn tracker_falls_back_to_local_when_console_line_inconsistent() {
        let mut tr = CwdTracker::default();
        tr.on_input(b"echo a#b", None);
        // console_line 提取截断后与本地行不一致 → 用本地行
        let cmds = tr.on_input(b"\r", Some("root@h:~# echo a#b"));
        assert_eq!(cmds, vec!["echo a#b"]);
    }

    #[test]
    fn tracker_backspace_and_ctrl_u() {
        let mut tr = CwdTracker::default();
        tr.on_input(b"cd /etx\x7f", None); // 删掉 x
        let cmds = tr.on_input(b"c\r", None);
        assert_eq!(cmds, vec!["cd /etc"]);

        let mut tr = CwdTracker::default();
        // Ctrl-U 清行后重输：提交的是清行后的内容
        let cmds = tr.on_input(b"junk\x15cd /a\r", None);
        assert_eq!(cmds, vec!["cd /a"]);
    }

    #[test]
    fn pipeline_and_background_cd_do_not_count() {
        let mut s = state_at("/root");
        apply_command("cd /etc | cat", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"), "管道里的 cd 在子 shell 执行");
        apply_command("cd /tmp &", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"), "后台化的 cd 不改变父 shell");
        apply_command("cd /a && ls | head", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/a"));
    }

    #[test]
    fn subshell_cd_does_not_leak_to_parent() {
        let mut s = state_at("/root");
        apply_command("bash", &mut s);
        apply_command("cd /tmp", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/tmp"), "子 shell 内的 cd 照常跟踪");
        apply_command("exit", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"), "exit 应回到父 shell 目录");
        // 嵌套两层：zsh → bash
        apply_command("zsh", &mut s);
        apply_command("cd /var", &mut s);
        apply_command("bash", &mut s);
        apply_command("cd /etc", &mut s);
        apply_command("exit", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/var"));
        apply_command("exit", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"));
        // bash -c 'cd /x' 不进子 shell、不压栈
        apply_command("bash -c 'cd /tmp'", &mut s);
        assert_eq!(s.cwd.as_deref(), Some("/root"));
        apply_command("exit", &mut s);
        assert!(s.cwd.is_none(), "顶层 exit 即会话结束，目录未知");
    }

    #[test]
    fn tracker_reset_and_escape_sequences() {
        let mut tr = CwdTracker::default();
        tr.on_input(b"cd /e", None);
        tr.reset();
        let cmds = tr.on_input(b"\r", None);
        assert!(cmds.is_empty(), "reset 后本地行已清，无 console_line 则为空");
    }
}
