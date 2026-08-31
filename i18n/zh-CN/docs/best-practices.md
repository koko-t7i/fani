# 最佳实践

本指南介绍了一条稳妥的流程，从首次本地翻译逐步过渡到无人值守的 GitHub 发布。在之前的每个阶段都获得干净的结果之前，请保留生成的 `fani init` 默认设置。

## 推荐的推出方案

1. **在本地启动：** 运行 `fani init`，保持修订和发布功能处于禁用状态，使用 `max_tasks = 10`，并将提供商并发数设为 1。
2. **在消耗令牌之前进行验证：** 先运行 `fani doctor`，然后运行 `fani status`。检查固定的源修订版本、已发现的文档、待处理单元和冲突。
3. **运行一次有界同步：** 运行 `fani sync`，检查已物化的目标和两份报告，并解决所有 `needs_human` 结果。
4. **添加仓库检查：** 配置确定性的文档构建或代码检查命令，并确认候选项能够通过这些检查。
5. **逐步提高质量和吞吐量：** 仅在翻译质量达到可接受水平后启用修订；在关注提供商限制和成本的同时，小幅逐步提高 `max_tasks` 和并发量。
6. **启用本地发布：** 启用候选分支创建，同时保持禁用远程推送和 GitHub 拉取请求。
7. **启用远程发布：** 为每种语言使用专用的稳定分支、最小权限凭据和必需的检查。
8. **熟悉恢复操作后再进行调度：** 启用定时器之前，请先练习使用 `adopt`、`discard`，以及处理退出代码 1、2 和 3。

此顺序可确保首次运行成本低且可回退。它还将翻译质量问题与 GitHub 权限、分支保护和调度器配置区分开来。

## 提供商和凭据

### 对常用服务优先使用内置提供程序

内置的 Anthropic、OpenAI、xAI 和 DeepSeek 支持只需提供商名称、模型和标准环境变量，无需适配器脚本或提供商 CLI。

|提供商|配置值|凭据变量|
| --- | --- | --- |
|Anthropic| `anthropic` | `ANTHROPIC_API_KEY` |
| OpenAI| `openai` | `OPENAI_API_KEY` |
|xAI| `xai` | `XAI_API_KEY` |
|DeepSeek| `deepseek` | `DEEPSEEK_API_KEY` |

官方提供商端点和凭据变量名称是固定的。这可防止仓库配置重定向标准提供商密钥。内置客户端还会拒绝重定向，并且不会继承代理环境变量。

OpenAI 和 OpenAI 兼容的原生 Agent 可以将 `reasoning_effort` 设置为 `none`、`minimal`、`low`、`medium`、`high` 或 `xhigh`。请确认所选提供商和模型支持该值。若要保留提供商的默认行为，请不要配置此项；fani 会完全省略该字段。此设置属于提供商指纹的一部分，因此更改推理强度后不会复用以其他强度执行的尝试。

仅将 `openai-compatible` 用于您信任的服务。为其指定明确的 HTTPS 端点和专用凭据变量，且该变量不得重复用于官方提供商：

```toml
[agents.primary]
provider = "openai-compatible"
model = "example-model"
endpoint = "https://models.example.com/v1/chat/completions"
api_key_env = "FANI_EXAMPLE_API_KEY"
concurrency = 1
timeout_s = 300
retries = 2
enabled = true
```

对于无法公开 OpenAI 兼容 HTTPS API 的私有集成，请使用 `command-json-v1`。该命令必须实现 fani 严格的版本化 JSON 请求和响应封装。仅允许其所需的环境变量：

```toml
[agents.primary]
adapter = "command-json-v1"
provider = "private"
model = "example-model"
cmd = ["fani-provider"]
env_allow = ["PRIVATE_PROVIDER_TOKEN"]
concurrency = 1
timeout_s = 300
retries = 2
enabled = true
```

### 不要将机密信息存入代码仓库

- 将密钥存储在进程环境、本地权限模式为 `0600` 的环境文件或 CI 密钥存储中。
- 切勿将密钥放入 `fani.toml`、命令参数、报告或已提交的 shell 文件中。
- 尽可能为每个环境和提供商使用单独的密钥；如果作业日志或代码仓库可能泄露了密钥，请轮换密钥。
- 仅授予 GitHub 凭据更新分支和创建拉取请求所需的仓库权限。
- 请在与 `fani sync` 相同的环境中运行 `fani doctor`；交互式检查成功并不能证明 systemd 或 CI 环境具有相同的凭据。

fani 诊断信息会有意省略提供商响应正文、提示词、翻译内容、凭据和环境变量值。使用其他工具封装 fani 时，请保持这一特性。

## 仓库布局

初始布局适用于大多数仓库：

```text
README.md
fani.toml
docs/
i18n/
  zh-CN/
    README.md
    docs/
.fani/
  fani.db
```

推荐的配置规则：

- 仅包含应翻译的源 Markdown。
- 排除生成的目标根目录，例如 `i18n/**`；否则，已翻译的文件可能会被识别为新的源文件。
- 排除归档文件、生成的 API 参考文档、供应商内容、变更日志或法律文件，除非它们被明确纳入范围。
- 在 `target_pattern` 中保留 `{lang}` 和 `{relpath}`，以避免语言和源路径发生冲突。
- 将 `data_dir` 保留在仓库根目录内，但置于发布目标之外。不要提交 `<data_dir>/fani.db`。
- 请将报告置于翻译目标路径之外。报告是可替换的视图，而非状态或发布输入。
- 仅当相关仓库共享相同的运行计划和凭据边界时，才使用同一配置。否则，请使用单独的文件并分别调用。

fani 从 `publish.source_ref` 读取源 Blob，而不是从可变的工作树源文件中读取。请将其设置为代表已批准源文档的分支或引用；在持续拉取更新的检出中，通常为 `origin/main`。

## 翻译质量和成本

从范围明确的工作开始：

```toml
max_tasks = 10
repair_budget = 2

[repo.quality]
revision = false
proofread = false

[agents.primary]
concurrency = 1
```

然后一次调整一个维度：

- 提高 `max_tasks` 的值，以更改单次运行可完成的最大工作量。退出代码 3 表示有界批处理已成功完成，但仍有后续工作待处理。
- 仅在确认提供商的速率限制、延迟和账户支出后，才提高并发量。
- 在基础翻译提示词和术语能够生成可接受的输出后，启用修订。
- 仅当校对功能的建议性结果已有明确的人工处理流程时，才启用该功能。
- 将重试次数保持在较低水平。对于确定性的配置、身份验证和响应格式错误，应予以修正，而不是重复调用。

在发布前配置仓库特定的检查。命令采用 argv 数组，而不是 shell 字符串：

```toml
[repo.documentation]
commands = [
  ["mdbook", "build"],
  ["markdownlint", "docs/zh-CN"],
]
timeout_s = 120
```

每条命令都在独立的固定源暂存环境中运行，精确叠加候选目标，且不使用提供商凭据。确保检查具有确定性、非交互性，并且不依赖可变的本地构建产物。

## 日常操作

正常的手动运行方式如下：

```bash
git fetch --prune origin
fani doctor
fani status
fani sync --report-dir ./reports
```

操作规则：

- 在规划之前获取已配置的源引用；fani 会有意解析该引用在本地 Git 中的视图。
- 每次出现非零退出后，请读取 `report.md`；当自动化流程需要结构化结果时，请保留 `report.json`。
- 退出代码为 3 后，重新运行 `fani sync`。已完成的尝试和规范状态会持久保存，因此下次运行时将从中断处继续，而不是从头开始。
- 将退出代码 1 视为决策队列，而不是基础设施事件。
- 将退出代码 2 视为操作故障，应触发警报并停止发布自动化流程。
- 避免通过删除 `.fani/fani.db` 来清除错误。它包含翻译记忆、规范字节、恢复意图和发布状态。
- 迁移长期运行的安装实例时，请一并备份数据库、存储库标识和配置。

### 人工编辑：采纳或舍弃

如果已实体化的目标与其记录的规范哈希值不同，fani 会报告 `HUMAN-EDIT`，且不会在不提示的情况下将其覆盖。

查看文件并明确选择：

```bash
fani adopt --repo product-docs --lang zh-CN
# or
fani discard --repo product-docs --lang zh-CN
```

当人工编辑正确且应成为规范的可信翻译记忆库内容时，请使用 `adopt`。采用操作仍会执行确定性验证。当编辑是误操作或已过时时，请使用 `discard`，以恢复上一个规范且已验证的目标文本。

在无人值守发布之前解决人工编辑内容。反复丢弃审核者的有意更改既浪费工作成果，也会破坏信任模型。

## Git 和 GitHub 发布

启用分阶段发布：

1. 设置 `[repo.publish].enabled = true` 和 `push = false`，并在本地检查候选分支/提交的行为。
2. 设置一个稳定的分支命名模式，例如 `i18n/{lang}`。不要每次运行都生成新的分支名称。
3. 仅在远程仓库和凭据均正确后启用 `push`。
4. 仅在同一运行时环境中成功执行 `gh auth status` 后，才启用 `[repo.publish.github]`。
5. 在依赖无人值守的拉取请求之前，请在基础分支上配置必需的检查和分支保护。

fani 使用类型化的目标文件允许列表，基于固定的源提交构建候选提交。远程更新采用比较并交换/带租约强制推送语义，并为每个仓库和语言维护一个稳定的开放拉取请求。它不会自动合并拉取请求。

在可行的情况下，使用专用的自动化身份。仅授予推送本地化分支以及创建或更新拉取请求所需的最低仓库权限；不要向翻译任务暴露发布、管理或其他无关的组织凭据。

## 使用 systemd 进行调度

位于 [`../../../systemd/`](../../../systemd/) 中的模板会运行一项每小时执行一次且带有随机延迟的用户服务。其打包路径与发行版安装路径一致：`~/.local/bin/fani`、`~/.config/fani/fani.toml` 和 `~/.local/state/fani/reports`。安装前：

- 如果 fani 或其配置安装在其他位置，请调整 `ExecStart`；
- 将每个已配置的仓库父目录以及报告/状态父目录添加到 `ReadWritePaths`；
- 将提供商凭据放入 `~/.config/fani/env`，并将权限模式设为 `0600`；
- 手动运行完全相同的 `ExecStart` 命令；
- 确认退出代码 1 和 3 为可接受结果，而退出代码 2 仍表示服务故障；
- 当签出未在其他位置更新时，请验证已配置的源引用是否由单独的可信进程获取。

安装并检查用户单元：

```bash
install -Dm644 systemd/fani.service "$HOME/.config/systemd/user/fani.service"
install -Dm644 systemd/fani.timer "$HOME/.config/systemd/user/fani.timer"
systemctl --user daemon-reload
systemctl --user enable --now fani.timer
systemctl --user list-timers fani.timer
journalctl --user -u fani.service -n 50
```

定时器不能替代告警。请监控退出代码 2 和重复出现的退出代码 1，并定期确认以退出代码 3 结束的运行最终会清空待处理队列。

## CI 和可信自动化

当你需要在不调用模型的情况下进行确定性规划时，请在常规的拉取请求 CI 中使用 `fani check`。配置验证仍要求已配置的提供商凭据变量存在，因此请仅为此只读命令提供一个非机密占位值：

```bash
ANTHROPIC_API_KEY=check-only-placeholder fani check --config ./fani.toml
```

请使用与已配置的官方提供商匹配的变量。此占位符之所以安全，仅仅是因为 `check` 从不与提供商通信；切勿将此模式复用于 `sync`。

不要在由不受信任的拉取请求触发的工作流中使用由模型支持的 `fani sync`。此类工作流可能会泄露机密、消耗提供商配额、发布由攻击者控制的分支，或使用受信任的凭据运行存储库中定义的文档命令。

更安全的拆分方式是：

- **拉取请求 CI：** 在没有提供商或 GitHub 写入凭据的情况下构建并测试 fani/配置变更；使用包含 `publish.source_ref = "HEAD"` 的专用 CI 配置，以便对已检出的拉取请求合并提交进行规划，并且仅在所需的本地仓库和状态可用时运行 `fani check`。
- **可信同步：** 通过受保护的默认分支上定时或手动触发的工作流运行 `fani sync`，并启用环境审批和最小权限密钥。
- **发布检查：** 使用常规的仓库文档测试来验证区域设置拉取请求，不依赖提供商访问权限。
- **发布 CI：** 将 fani 自身的格式检查、Clippy、测试、最低 Rust 版本、依赖项审计、策略和可复现性门禁与翻译同步分开。

SQLite 是持久的应用程序状态，而不是可随意丢弃的依赖项缓存。托管运行器必须安全地还原并持久保存完全一致的数据库，或者使用长期运行且受保护的运行器。切勿让并发作业写入同一数据库或区域设置发布状态。

### 仓库工作流模板

[`.github/workflows/provider-sync.yml`](../../../.github/workflows/provider-sync.yml) 是一个可信的默认分支状态持久化模板，而非通用的即插即用发布工作流。它仅在推送到默认分支、按计划运行或手动触发时运行；按仓库对运行进行串行化；从 `refs/heads/fani-state` 恢复权威 SQLite 文件；并在同步后使用比较并交换机制发布该文件。

启用前：

- 将仓库变量 `FANI_CONFIG_PATH` 设置为 CI 专用配置；
- 确保 `FANI_STATE_DB` 与所选仓库的 `<data_dir>/fani.db` 完全匹配；
- 将配置限制为一个持久化存储库/数据库，或者在调用 fani 时选择存储库，并为每个额外的数据库创建单独的受保护状态设计；
- 将 `fani init` 生成的本地仓库绝对路径替换为在运行器检出目录中有效的路径；
- 保持禁用 `publish.push = false` 和 GitHub 拉取请求发布，除非你单独向提供程序步骤添加权限范围严格受限的 Git/`gh` 身份验证；
- 仅添加已配置的提供商所需的凭据。

签入的模板目前仅注入 `ANTHROPIC_API_KEY`。OpenAI、xAI、DeepSeek、OpenAI 兼容服务和自定义 Agent 需要显式配置密钥和环境变量。避免将所有提供商的密钥注入同一个作业；请谨慎选择提供商，并且仅公开其凭据。

将 `fani-state` 视为受保护的应用程序状态，而非缓存。限制可更新或删除它的人员，对其进行备份，并且切勿持久化 SQLite 日志、WAL 或共享内存辅助文件。工作流的 `contents: write` 权限由状态恢复/发布步骤使用；签出凭据不会被持久化，提供程序步骤也不会收到 Git 或 `gh` 发布凭据。

## 故障排除顺序

当运行失败时，请按以下顺序进行诊断：

1. `fani doctor` 用于检查配置、凭据、Git、SQLite、自定义 Agent 和 GitHub 的先决条件。
2. `fani status` 用于查看已解析的源修订版本和确定性规划冲突。
3. `report.md` 和 `report.json` 用于阶段和决策代码。
4. `FANI_LOG=info` 用于经过脱敏处理的运行诊断。
5. 对于仅在发布时出现的故障，请检查 Git 远程仓库和 `gh auth status`。

当日志收集器需要以换行符分隔的 JSON 时，请使用 `FANI_LOG_FORMAT=json`。不要仅仅为了更方便地检查故障而添加会输出环境变量、提供商有效载荷或翻译内容的包装器。
