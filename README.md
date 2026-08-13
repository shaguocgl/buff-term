<p align="center">
  <img src="src/assets/keywisp-logo.svg" width="128" alt="KeyWisp logo" />
</p>

# KeyWisp Agent

> 本地优先的桌面 SSH 管理 + AI Agent 工具
>
> A local-first desktop SSH manager with an AI agent that manages your servers through natural language.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey.svg)
![Rust](https://img.shields.io/badge/Rust-1.97-orange.svg)
![React](https://img.shields.io/badge/React-19-blue.svg)
![Agent](https://img.shields.io/badge/Agent-自研编排%20%7C%20无框架依赖-8b5cf6.svg)

KeyWisp Agent 是一款桌面端 SSH 管理工具，内置**自研的 AI Agent 编排层**：你可以自行配置大模型平台（DeepSeek、OpenAI、通义千问、Kimi、本地 Ollama 等），让 AI 通过自然语言帮你查询和运维远程服务器。所有配置、密钥和审计数据默认只保存在本机。

> 🤖 **Agent 编排层完全自研**：核心工具调用循环（SSE 流式解析 → 工具执行 → 结果回填）由本项目手写实现，**不依赖 LangChain / OpenAI Agents SDK / Vercel AI SDK 等框架**——审批拦截、安全策略、审计留痕全部自己掌控。详细设计见 [实现细节](docs/实现细节.md)。

## ✨ 功能特性

### SSH 基础管理

- 主机配置增删改查（名称、地址、端口、用户名、认证方式）
- 密钥 / 密码认证，密码与 API Key 存入系统钥匙串（macOS Keychain / Windows 凭据管理器 / Linux Secret Service）
- 多标签终端会话（xterm.js）、窗口尺寸自适应
- 一键导入 `~/.ssh/config`（HostName / User / Port / IdentityFile）
- 密码认证由后端直接注入钥匙串凭据，不经过终端提示符识别

### AI Agent

- **自研 Agent 编排层**：SSE 流式解析、工具调用循环、审批与审计均为手写实现，无框架依赖，行为完全可控
- **russh 协议级执行**：AI 工具调用走独立 SSH 连接（连接复用、known_hosts 校验、结构化 stdout/退出码）
- 多平台配置：DeepSeek / OpenAI / 通义千问 / Kimi / Ollama（OpenAI 兼容协议）
- 单个厂商支持配置多个模型，聊天窗口底部可随时切换
- 流式回复，Markdown 渲染（代码块 / 表格 / 列表）
- 工具调用：`exec_command`、`read_file`、`list_dir`、`resource_usage`
- 会话内多轮上下文，可随时中断 / 清空

### MCP 服务

- **KeyWisp 作为 MCP 服务器对外提供服务**：基于 Streamable HTTP + JSON-RPC + token 认证
- 把勾选的服务器能力开放给 Codex、Claude Desktop 等外部 AI 工具
- 提供 `list_hosts`、命令执行、文件读取、目录列表、资源查询等 SSH 工具
- 支持三种权限模式：
  - **只读模式**：禁止写操作，适合安全查询场景
  - **管控模式**：系统预置 + 自定义危险命令规则，命中后需人工确认
  - **全部放行**：可执行任意命令，适合完全信任场景
- 启动后自动生成外部 AI 的 MCP 配置 JSON，支持 token 轮换与吊销

### 安全体系

- 三级安全级别：
  - **全部审核**：每个工具调用都需要人工批准
  - **智能审核**：只读命令自动执行；危险命令（内置规则 + 自定义规则 + 模型判定）需批准
  - **全部放行**：命令直接执行（谨慎使用）
- 自定义智能审核规则：子串匹配，无需通配符，如配置 `rm -rf` 即可命中所有包含该片段的命令
- 审计日志：每次 AI 工具调用的时间、主机、命令、审批方式、结果摘要、耗时，面板内可查
- 输出脱敏：命令输出进入模型前过滤 AK/SK、密钥、口令、私钥块等敏感信息
- 私钥与 API Key 永不进入模型上下文，认证由后端注入

### 监控、巡检与通知

- 监控面板：CPU / 内存 / 磁盘仪表盘 + 负载 + TOP 进程，每 5 秒自动刷新
- AI 巡检：采集服务器基线数据后由 AI 生成中文 Markdown 报告；仅允许白名单内的只读命令，支持取消、历史报告与风险等级
- 通知配置：邮件（SMTP）发送渠道配置，支持测试连接
- 巡检报告可自动转为 HTML 并发送至已配置的邮件收件人
- 版本检查：侧边栏可显示当前版本并检查 GitHub Releases 的最新正式版本，发现更新后可跳转下载

## 🧱 技术栈

| 层次 | 技术 |
| --- | --- |
| 桌面框架 | Tauri 2（Rust 后端） |
| 前端 | React 19 + TypeScript + xterm.js + Vite |
| 交互终端 / SFTP / AI / MCP / 监控 | russh + russh-sftp（协议级 SSH，全部能力走同一套连接实现） |
| 存储 | SQLite（rusqlite）+ 系统钥匙串（keyring） |
| AI 接入 | OpenAI 兼容协议，SSE 流式解析，自研工具调用循环 |
| MCP 服务 | 自研 Streamable HTTP + JSON-RPC 服务器（tiny_http） |

## 🏗 架构

```mermaid
flowchart LR
  UI["React + xterm.js<br/>终端 + 聊天面板"] -->|Commands / Events| BE["Rust 后端 (Tauri)"]
  BE --> SM["Session Manager"]
  BE --> AG["AI Agent Runtime"]
  BE --> MCP["对外 MCP 服务<br/>Streamable HTTP + token"]
  MCP --> MCPTOOL["工具层<br/>list_hosts / exec / 读文件 / 列目录 / 资源查询"]
  SM --> SSH["交互会话<br/>russh shell channel"]
  AG --> TOOL["工具层<br/>exec / 读文件 / 列目录 / 资源查询"]
  TOOL --> RSH["russh 连接池<br/>协议级执行 / 连接复用"]
  BE --> INSP["AI 巡检<br/>只读命令 + 报告归档"]
  INSP --> RSH
  INSP --> PROV
  AG --> PROV["模型适配层<br/>OpenAI 兼容协议"]
  PROV --> DS["DeepSeek"]
  PROV --> QW["通义 / Kimi / OpenAI"]
  PROV --> OLL["本地 Ollama"]
  BE --> DB[("SQLite<br/>配置 / 规则 / 审计")]
  BE --> KC[("系统钥匙串<br/>密码 / API Key")]
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

### 自动发布

推送与应用版本一致的 Git 标签（例如当前版本使用 `v0.1.0`）会触发 GitHub Actions：自动构建 macOS 通用 DMG（Intel + Apple Silicon）与 Windows x64 NSIS 安装包，并上传至同名 GitHub Release。发布前请确保 `package.json` 与 `src-tauri/tauri.conf.json` 中的版本号一致。

```bash
git tag v0.1.0
git push origin v0.1.0
```

## 📖 使用说明

1. **添加主机**：左侧「新建主机」，填写地址、用户名，选择密钥或密码认证（密码会存入系统钥匙串）；也可以点「导入 ~/.ssh/config」一键导入。
2. **连接**：点击主机卡片即可连接，首次连接按终端提示确认主机指纹。
3. **配置 AI**：点击底部「AI Agent」卡片，选择平台预设（如 DeepSeek），填写 API Key 并「测试连接」，保存后即可使用。
4. **AI 对话**：连接服务器后右侧出现聊天面板；底部可切换安全级别（默认智能审核）与当前模型。
5. **AI 巡检**：连接服务器后点击终端工具栏的「巡检」，系统采集只读基线数据并生成报告；配置 SMTP 后会自动发送 HTML 报告。
6. **MCP 服务**：侧边栏「MCP 服务」勾选要开放的服务器 → 选择权限模式 → 启动，复制生成的配置 JSON 粘贴到 Codex / Claude Desktop 即可接入。
7. **操作日志与更新**：侧边栏底部可查看所有 AI / MCP 工具调用记录；「检查更新」会查询 GitHub 最新正式 Release，发现版本后可跳转下载。

## 📚 文档

- [实现细节](docs/实现细节.md)：代码结构、核心实现与最新实现说明

## 📁 项目结构

```text
src/                   前端（React + xterm.js）
  components/          聊天面板 / 终端 / 弹窗 / 下拉框等
  assets/              KeyWisp 界面与文档 logo
src-tauri/src/         Rust 后端
  agent.rs             AI Agent 运行时（流式解析、工具循环、审批、审计）
  session.rs           SSH 交互会话（russh shell channel）
  russh.rs             russh 连接池（AI / MCP 工具执行，连接复用）
  hosts.rs             主机配置
  ai.rs                AI 平台 / 模型 / 审核规则配置
  credentials.rs       系统钥匙串凭据 + 内存缓存
  audit.rs             审计日志查询
  monitor.rs           资源快照采集（CPU / 内存 / 磁盘 / 负载 / TOP 进程）
  alert.rs             通知配置（邮件 SMTP 配置与测试）
  inspection.rs        AI 只读巡检、报告生成与邮件投递
  mcp.rs               对外 MCP 服务（HTTP + token + 权限模式）
  sftp.rs              SFTP 文件操作（russh-sftp）
  update.rs            GitHub Release 版本检查
  db.rs                SQLite（主机、AI 配置、规则、审计）
src-tauri/icons/       桌面应用图标（PNG / ICNS / ICO）
.github/workflows/     GitHub Actions 自动构建与发布
docs/                  实现细节
```


## 🔒 安全说明

- 密码与 API Key 仅存于系统钥匙串，数据库只存引用，不落盘明文；
- AI 会话的认证由后端注入，私钥、密码、API Key 不会进入模型上下文；
- 危险命令默认需要人工批准；审计日志帮助追溯 AI 的每次操作；
- 发布版请使用正式签名（macOS 公证 / Windows 代码签名），以消除开发版钥匙串授权弹窗。

## 🤝 贡献

欢迎提交 Issue 与 Pull Request。

## 📄 License

[MIT](LICENSE)
