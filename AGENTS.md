# AGENTS.md

This file guides AI coding agents (Codex, Claude Code, etc.) working in this repository.

## 提交身份规范（Commit identity）

本项目只允许仓库所有者的 git 身份出现在提交记录中；禁止任何 AI 工具 / 平台的作者或署名注入。

- 提交的作者（author）与提交者（committer）必须使用本机 `git config user.name / user.email`（当前为 `shaguocgl <shaguocgl@users.noreply.github.com>`）。
- 禁止在提交信息中加入任何 `Co-Authored-By:` / `Signed-off-by:` / `Reported-by:` 等署名行；尤其禁止加入 Claude Code、Codex、GitHub Copilot、Cursor 等 AI 工具的作者或署名。
- 禁止通过 `GIT_AUTHOR_*` / `GIT_COMMITTER_*` 环境变量把作者或提交者改为 `codex`、`claude`、`copilot` 等非用户身份。
- 提交前如发现提交信息中已有上述署名行，必须先删除。
- 本仓库已配置 `.git/hooks/commit-msg` 钩子，在 git 层自动移除 `Co-Authored-By:` 行作为兜底。

## Git 写操作必须显式授权（Git write authorization）

未经仓库所有者明确同意，禁止执行任何会改变 git 历史、标签或远端状态的操作。

- 需要逐次获得明确同意的操作：`git add`、`git commit`、`git tag`、`git push`、`git reset`、`git revert`、`git merge`、`git rebase`、创建/合并 PR、删除分支或标签、触发发布或构建等。
- “按方案实现”“完成发布”“把功能做完”等笼统表述不构成提交授权；必须得到类似“请提交并推送”“可以提交了”的明确指令。
- 代码编辑、读取、搜索、测试、构建检查等非 git 写操作可以正常进行。
- 如果用户只要求实现功能，完成后应停在“改动已完成，等待你确认是否提交”，不得自行提交。
- 如果用户要求撤销或回退，先说明影响（是否改写远端历史、是否需要 force push），得到确认后再执行。
