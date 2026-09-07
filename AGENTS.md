# AGENTS.md

This file guides AI coding agents (Codex, Claude Code, etc.) working in this repository.

## 提交身份规范（Commit identity）

本项目只允许仓库所有者的 git 身份出现在提交记录中；禁止任何 AI 工具 / 平台的作者或署名注入。

- 提交的作者（author）与提交者（committer）必须使用本机 `git config user.name / user.email`（当前为 `shaguocgl <shaguocgl@users.noreply.github.com>`）。
- 禁止在提交信息中加入任何 `Co-Authored-By:` / `Signed-off-by:` / `Reported-by:` 等署名行；尤其禁止加入 Claude Code、Codex、GitHub Copilot、Cursor 等 AI 工具的作者或署名。
- 禁止通过 `GIT_AUTHOR_*` / `GIT_COMMITTER_*` 环境变量把作者或提交者改为 `codex`、`claude`、`copilot` 等非用户身份。
- 提交前如发现提交信息中已有上述署名行，必须先删除。
- 本仓库已配置 `.git/hooks/commit-msg` 钩子，在 git 层自动移除 `Co-Authored-By:` 行作为兜底。
