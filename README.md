<p align="center">
  <img src="src/assets/buffterm-logo.png" width="128" alt="buffTerm logo" />
</p>

# buffTerm

> AI Agent Buff 加持的 SSH 管理工具
>
> A desktop SSH manager supercharged by a self-built AI agent — manage servers through natural language, locally.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey.svg)
![Rust](https://img.shields.io/badge/Rust-1.97-orange.svg)
![React](https://img.shields.io/badge/React-19-blue.svg)
![Agent](https://img.shields.io/badge/Agent-自研编排%20%7C%20无框架依赖-8b5cf6.svg)
[![Website](https://img.shields.io/badge/官网-buffTerm%20Site-22d3ee.svg)](https://shaguocgl.github.io/buff-term-site/)

buffTerm 是一款有 AI Agent Buff 加持的 SSH 管理工具，——内置**自研 AI Agent 编排层**，接入任意支持open api协议的大模型（如DeepSeek、Qwen、kimi等），用自然语言就能 查询状态、排查问题、安装程序、开展运维工作。

并支持将 多个主机 同时暴露为 MCP 服务，提供给其他 AI 工具调用，安全级别由你把关。高危命令拦截、AI 操作审批与审计全程兜底，所有数据只存本机。

> 🤖 **Agent 编排层完全自研**：核心工具调用循环（SSE 流式解析 → 工具执行 → 结果回填）由本项目手写实现，**不依赖 LangChain / OpenAI Agents SDK / Vercel AI SDK 等框架**——审批拦截、安全策略、审计留痕全部自己掌控。
<p align="center">
  <img src="images/ai-agent.webp" alt="AI Agent 会话与终端" width="100%" />
</p>
<p align="center">
  <a href="https://shaguocgl.github.io/buff-term-site">
    <img src="images/desc.webp" alt="buffTerm 功能介绍（点击访问官网）" width="100%" />
  </a>
</p>

## 📥 下载

请前往 [GitHub Releases](https://github.com/shaguocgl/buff-term/releases) 下载最新安装包：

- macOS：下载 `.dmg`（Universal，同时支持 Apple Silicon 与 Intel）
- Windows：下载 `.exe`

> ⚠️ macOS 版本目前未做 Apple 公证。凭据以 AES-256-GCM 加密存本机 SQLite，主密钥存系统钥匙串；未签名开发版首次访问钥匙串时可能弹一次系统授权提示，属 macOS 正常行为。

## ✨ 功能特性

### SSH 与 SFTP

- **协议级 SSH**：交互终端、AI、MCP、监控、巡检与 SFTP 全部基于 `russh` / `russh-sftp`，不调用系统 `ssh` / `sftp` 命令
- **多种认证方式**：支持密码、keyboard-interactive 回退、普通/加密私钥、Unix ssh-agent；RSA 自动协商 SHA-2 签名
- **严格主机指纹校验**：首次连接展示 SHA-256 指纹并等待确认，已记录指纹变化时直接拒绝并显示新旧指纹
- **并发连接复用**：AI、MCP、监控、巡检和 SFTP 按主机复用 SSH 连接，通道级并发，空闲连接自动回收
- **结构化 SFTP**：目录浏览、上传、下载、重命名、删除、建目录、覆盖确认、进度与取消；支持符号链接和含空格文件名
- **批量上传**：文件选择支持多选，最多 3 个并发传输（每条独立进度与取消，可一键全部取消）；冲突文件汇总为一次覆盖确认，可选择「全部覆盖」或「跳过冲突项」仅上传其余文件
- **内置文件编辑器**：文件面板行内悬浮按钮（或双击文件行）即可在中间区以标签页打开文件（CodeMirror 6，按扩展名高亮），支持编辑与 Ctrl/Cmd+S 保存；保存采用「同目录临时文件 + rename」原子替换并沿用原权限，写入前校验 mtime 避免冲掉他人改动；符号链接写入落到真实目标而不替换链接；单文件 2 MiB 上限并对二进制 / 非 UTF-8 文件直接拒绝；`~/.ssh`、`/etc/sudoers`、`/etc/shadow`、cron、systemd、登录 shell 启动脚本等敏感路径需二次确认，每次保存写入操作审计

### 终端防护

- **回车前拦截高危命令**：基于终端实际命令行判定（覆盖 Tab 补全 / 方向键历史 / 编辑键），命中规则弹窗确认后才执行
- **预置 + 自定义危险规则**：覆盖 `rm -rf`、`mkfs`、`dd if=`、`systemctl stop`、`drop table`、`curl | sh` 等，可删改、可一键恢复
- 全屏应用（vim / htop 等）内自动透传，不误判编辑内容；每次拦截写入审计日志

### AI Agent

- **自研 Agent 编排层**：SSE 流式解析、工具调用循环、审批与审计均为手写实现，无框架依赖
- **russh 协议级执行**：AI 工具调用走统一 SSH 连接池（连接复用、通道并发、known_hosts 校验）
- 多平台配置：DeepSeek / OpenAI / 通义千问 / Kimi / Ollama，单厂商可配多模型
- 工具调用：`exec_command`、`read_file`、`list_dir`、`resource_usage`、`query_history`（历史指标趋势，支持分钟 / 小时 / 天三档聚合粒度，AI 按分析目的自选）、`update_task_plan`（本地任务台账，免审批）
- 发送上下文按模型窗口做预算：旧轮次自动压缩摘要，任务台账始终保留
- 每台主机独立会话历史，可随时中断 / 清空

### MCP 服务

- **buffTerm 作为 MCP 服务器**：基于 Streamable HTTP + JSON-RPC + token 认证，把勾选的服务器能力开放给 Codex、Claude Desktop 等外部 AI 工具
- 三种权限模式：只读（禁止写操作）/ 管控（危险命令需确认）/ 全部放行
- 启动后自动生成外部 AI 的 MCP 配置 JSON，支持 token 轮换与吊销

### 安全体系

- 三级安全级别：全部审核 / 智能审核（只读自动执行，危险命令需批准）/ 全部放行
- 凭据加密：SSH 密码 / 私钥口令、AI API Key、MCP token、SMTP 密码统一 AES-256-GCM 加密存本机，主密钥存系统钥匙串；私钥口令仅用于本地解密私钥
- 操作审计：记录 AI Agent、终端防护、MCP 服务、修复执行与文件编辑的时间、主机、来源、命令、审批方式与结果摘要
- 输出脱敏：命令输出进入模型前过滤 AK/SK、密钥、口令等敏感信息；私钥与 API Key 永不进入模型上下文

### 监控、巡检与通知

- 监控面板：CPU / 内存 / 磁盘仪表盘 + 负载 + TOP 进程，自动刷新
- AI 巡检：采集只读基线数据生成中文 Markdown 报告，含木马 / 挖矿风险检测
- 一键整改：AI 结合巡检报告生成整改步骤，确认后自动执行，支持步骤编辑与失败重试
- 邮件通知：巡检报告与整改结果可自动发送至已配置的收件人

## 🧱 技术栈

| 层次 | 技术 |
| --- | --- |
| 桌面框架 | Tauri 2（Rust 后端） |
| 前端 | React 19 + TypeScript + xterm.js + CodeMirror 6 + Vite |
| 交互终端 / SFTP / AI / MCP / 监控 | russh + russh-sftp（协议级 SSH，全部能力走同一套连接实现） |
| 存储 | SQLite（rusqlite）存配置 / 审计 / 加密凭据 + 系统钥匙串（keyring）存主密钥 |
| AI 接入 | OpenAI 兼容协议，SSE 流式解析，自研工具调用循环 |
| MCP 服务 | 自研 Streamable HTTP + JSON-RPC 服务器（tiny_http） |

## 🏗 架构

```mermaid
flowchart LR
  UI["React + xterm.js<br/>终端 + 聊天面板"] -->|Commands / Channel / Events| BE["Rust 后端 (Tauri)"]
  BE --> SM["Session Manager"]
  BE --> AG["AI Agent Runtime"]
  BE --> MCP["对外 MCP 服务<br/>Streamable HTTP + token"]
  MCP --> MCPTOOL["工具层<br/>list_hosts / exec / 读文件 / 列目录 / 资源查询"]
  SM --> SSH["交互会话<br/>russh shell channel + 二进制 Channel 输出"]
  AG --> TOOL["工具层<br/>exec / 读文件 / 列目录 / 资源查询 / 历史趋势"]
  TOOL --> RSH["russh 连接池<br/>协议级执行 / 连接复用 / 通道并发"]
  BE --> SFTP["SFTP<br/>结构化目录 + 分块传输"]
  SFTP --> RSH
  BE --> INSP["AI 巡检<br/>只读命令 + 报告归档"]
  INSP --> RSH
  INSP --> PROV
  AG --> PROV["模型适配层<br/>OpenAI 兼容协议"]
  PROV --> DS["DeepSeek"]
  PROV --> QW["通义 / Kimi / OpenAI"]
  PROV --> OLL["本地 Ollama"]
  BE --> DB[("SQLite<br/>配置 / 规则 / 审计 / 加密凭据 / 历史指标")]
  BE --> KC[("系统钥匙串<br/>主密钥")]
  BE --> MON["监控采集<br/>russh 连接池"]
```

## 🚀 快速开始

### 环境要求

- Node.js 20+
- Rust stable（1.97+）
- macOS 需要 Xcode Command Line Tools；Windows 需要 WebView2（一般已内置）

### 开发运行

```bash
npm install
npm run tauri dev
```

### 验证

```bash
npm run build
cd src-tauri
cargo check
cargo test
```

### 打包

```bash
npm run tauri build
```

常用安装包快捷命令：

```bash
# macOS DMG
npm run build:mac

# Windows NSIS EXE
npm run build:win
```

产物位置：

```text
src-tauri/target/release/bundle/dmg/
src-tauri/target/release/bundle/nsis/
```

> DMG 需要在 macOS 上构建，EXE 建议在 Windows 上构建；打包前会先执行前端构建与 Rust 编译。未配置代码签名时仍可打包，但系统可能显示安全提示。

## 📚 文档

- [凭据存储加密设计](docs/密码存储加密设计.md)：凭据 AES-256-GCM 加密与主密钥管理
- [自研 AI Agent 与权限设计](docs/自研AI-Agent与权限设计.md)：Agent 运行时、审批与安全级别、AI 配置
- [对外 MCP 服务设计](docs/对外MCP服务设计.md)：Streamable HTTP 服务、权限模式与接入
- [AI 巡检整改功能设计](docs/AI巡检整改功能设计.md)：巡检、一键整改与通知（邮件）
- [终端危险命令拦截设计](docs/终端危险命令拦截设计.md)：前端权威命令行 + 后端判定的拦截实现

## 📁 项目结构

```text
src/                   前端（React + xterm.js）
  components/          聊天面板 / 终端 / 文件编辑器 / 弹窗 / 下拉框等
  assets/              buffTerm 界面与文档 logo
src-tauri/src/         Rust 后端
  agent.rs             AI Agent 运行时（流式解析、工具循环、审批、审计）
  agent/tools.rs       Agent 工具定义与执行（系统提示词、工具 schema、exec/read_file/list_dir/resource_usage/query_history/update_task_plan）
  agent/trend.rs       历史指标趋势分析（线性回归、分钟/小时/天粒度聚合、趋势文本格式化）
  safety.rs            安全判定与脱敏（危险命令 / 只读检测 / 输出脱敏）
  guard.rs             终端危险命令拦截（行缓冲状态机 + 规则判定 + 审批）
  util.rs              通用工具函数（时间戳 / 截断 / shell 转义 / token）
  session.rs           SSH 交互会话（二进制 Channel 输出、退出/断线状态）
  russh.rs             统一连接池（认证、known_hosts、并发通道、exec）
  hosts.rs             主机配置
  ai.rs                AI 平台 / 模型 / 审核规则配置
  credentials.rs       凭据加密（AES-256-GCM）：主机密码 / 私钥口令 / API Key / MCP token / SMTP 密码
  audit.rs             审计日志查询
  monitor.rs           资源快照采集（CPU / 内存 / 磁盘 / 负载 / TOP 进程），写入历史指标表供 query_history 分析
  alert.rs             通知配置（邮件 SMTP 配置与测试）
  inspection.rs        AI 只读巡检（含木马 / 挖矿风险采集）、报告生成与邮件投递
  remediation.rs       一键整改（整改步骤生成、执行、重试、审计与邮件通知）
  mcp.rs               对外 MCP 服务（HTTP + token + 权限模式）
  sftp.rs              SFTP 文件操作：结构化目录、分块传输与内置编辑器读写（原子写 / 敏感路径 / 审计）
  update.rs            GitHub Release 版本检查
  db.rs                SQLite（主机、AI 配置、规则、审计、巡检与整改、历史指标）
src-tauri/icons/       桌面应用图标（PNG / ICNS / ICO）
.github/workflows/     GitHub Actions 自动构建与发布
docs/                  设计文档（密码加密 / AI Agent 权限 / MCP 服务 / 巡检整改 / 终端拦截）
```


## 📄 License

[MIT](LICENSE)
