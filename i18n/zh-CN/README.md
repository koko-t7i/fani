# fani

[English](../../README.md)

fani 是一款 Linux 优先的命令行工具，用于持续翻译 Markdown 文档。它从固定的 Git 提交中读取源文件，复用 SQLite 中可信的翻译，仅将未解决的单元发送给内置模型提供商或严格的自定义 Agent，验证翻译结果，并可为每种语言发布一个稳定分支和一个 GitHub 拉取请求。

发布的二进制文件完全由原生 Rust 编写。运行 fani 不需要 Python、`uv`、外部 i18n 技能或提供商适配器脚本。

## 要求

- Linux;
- Git;
- Anthropic、OpenAI、xAI 或 DeepSeek 的 API 密钥，或者实现自定义 Agent 协议的命令；
- `gh` 仅在启用 GitHub 拉取请求发布时使用；
- 仅从源代码构建时需要 Rust 1.85 或更高版本。

## 安装

对于使用 glibc 2.31 或更高版本的 Intel/AMD 64 位 Linux：

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koko-t7i/fani/releases/latest/download/fani-installer.sh | sh
```

无需安装 Rust 工具链。有关经过验证的下载或源码安装，请参阅[发布与安装](docs/release.md)。

## 五分钟启动法

无需提供商脚本或提供商 CLI。

提交要翻译的源 Markdown，然后运行：

```bash
export ANTHROPIC_API_KEY='...'
fani init --lang zh-CN --provider anthropic --model claude-sonnet-4-5
fani doctor
fani status
fani sync
```

`fani init` 会为当前仓库创建一个安全且仅限本地使用的 `fani.toml`。生成的 glob 模式会在后续规划期间发现 Markdown 文件，目标文件位于 `i18n/<language>/` 下，每次运行最多执行十个任务，并且修订和发布功能保持禁用。除非指定 `--force`，否则它会拒绝覆盖现有配置。

`fani status` 无需调用模型即可预览固定的 Git 修订版本和计划的工作。未提交的源文件编辑不属于该修订版本。首次执行 `sync` 时，会生成目标文件，将权威状态存储在 `.fani/fani.db` 中，并写入 `.fani-report/report.md` 和 `.fani-report/report.json`；它不会推送或创建拉取请求。退出代码 3 表示有限运行已成功完成，应再次执行 `fani sync` 以继续完成剩余工作。

将 `.fani/` 和 `.fani-report/` 添加到 `.gitignore`。在启用成本更高的质量阶段、远程推送、GitHub 拉取请求或无人值守调度之前，请检查生成的翻译和 `report.md`。

### 内置提供商

|提供商| `--provider` |凭据环境变量|
| --- | --- | --- |
|Anthropic| `anthropic` | `ANTHROPIC_API_KEY` |
|OpenAI| `openai` | `OPENAI_API_KEY` |
|xAI| `xai` | `XAI_API_KEY` |
|DeepSeek| `deepseek` | `DEEPSEEK_API_KEY` |

对于自定义的 OpenAI 兼容服务，请使用 `provider = "openai-compatible"`，并明确指定 HTTPS `endpoint` 和专用的 `api_key_env`。高级私有集成可以使用严格的 `command-json-v1` 子进程协议。请将所有凭据保存在环境变量或密钥存储中，切勿保存在 `fani.toml` 中。

有关这两种方式，请参阅[最佳实践](docs/best-practices.md#提供商和凭据)和[带注释的配置](../../examples/fani.toml)。

## 核心工作流程

```bash
fani doctor
fani status
fani sync
```

|命令|目的|
| --- | --- |
| `fani init` |为一个内置提供商创建一套保守的初始配置。|
| `fani doctor` |验证配置、代码仓库、提供商凭据或自定义命令、SQLite，以及可选的 GitHub 先决条件。|
| `fani status` |无需调用模型，根据固定的源修订进行规划。|
| `fani check` |使用面向 CI 的名称运行相同的只读规划路径。|
| `fani sync` |恢复或执行翻译、验证、具体化以及可选的发布。|
| `fani adopt` |验证人工编辑的目标内容，并将其设为规范的可信内容。|
| `fani discard` |将存在差异的人工编辑替换为上次经过验证的规范目标。|

需要时，选择一个已配置的仓库或语言：

```bash
fani status --config ./fani.toml --repo product-docs --lang zh-CN
fani sync --config ./fani.toml --report-dir ./reports --quiet
fani adopt --repo PATH_OR_BASENAME --lang zh-CN
```

`fani sync` 将可替换的 `report.json` 和 `report.md` 视图写入选定的报告目录。位于 `<repo>/<data_dir>/fani.db` 的 SQLite 仍是唯一由 fani 管理的状态权威数据源。

### 退出代码

|退出|结果|含义|
| ---: | --- | --- |
| 0| `ok` |已是最新状态，或已完成并验证。|
| 1| `needs_human` |冲突、无效候选项、审查发现或发布决策需要人工处理。|
| 2| `error` |配置或基础设施失败。|
| 3| `partial` |有限批次已成功完成，仍有更多单元留待后续运行。|

对于多个仓库或语言，优先级为 `error > needs_human > partial > ok`。

## 安全模型

- **固定来源：** 发现和规划从一个已解析的 Git 提交中读取 blob，而绝不读取工作树中可变的源文件。
- **单一状态权威来源：** 翻译记忆、检查结果、规范目标字节、恢复状态和发布状态均存储在 SQLite 中。
- **不受信任的模型输出：** fani 保护 Markdown 语法、限制提供商的 I/O，并在具体化或发布之前验证候选内容。
- **显式人工协调：** 已更改的目标绝不会被静默覆盖；请选择 `adopt` 或 `discard`。
- **隔离发布：** 候选提交使用临时 Git 索引，不会影响已检出的分支、`HEAD`、实际索引或无关文件。
- **机密信息不写入配置：** 官方提供商使用固定端点和固定的凭据变量名称；内置请求不会跟随重定向，也不会继承代理环境变量。

## 配置

[`examples/fani.toml`](../../examples/fani.toml) 是完整的带注释参考文档。未知字段会被拒绝，并且语义验证会在 fani 联系提供商或 GitHub 之前完成。

重要规则：

- `publish.source_ref` 选择用于规划和同步的固定源修订版本，即使发布功能已禁用；
- 排除生成的翻译目录，以免对其进行递归翻译；
- 在 `target_pattern` 中保留 `{lang}` 和 `{relpath}`——例如，`i18n/{lang}/{relpath}` 会将 `docs/start.md` 映射到 `i18n/zh-CN/docs/start.md`；
- 将文档检查配置为 argv 数组，而不是 shell 字符串；
- 从较低的 `max_tasks`、`concurrency = 1`、禁用修订和禁用发布开始；
- 启用发布时，每种语言使用一个稳定的发布分支。

分阶段发布和运维指南请参阅[最佳实践](docs/best-practices.md)。

## 诊断

诊断信息需选择启用，并且仅输出到 stderr：

```bash
FANI_LOG=info fani sync --quiet
FANI_LOG=info FANI_LOG_FORMAT=json fani sync --quiet
```

它们包括安全标识符、持续时间、状态，以及提供商/发布元数据的哈希值。它们不包括凭据、环境值、提示词、源 Markdown、译文、受保护的令牌，以及提供商请求/响应正文。

## 文档

- [文档导航](docs/README.md) — 说明应使用哪些文档以及哪些契约是最新的。
- [最佳实践](docs/best-practices.md) — 提供商、凭据、仓库布局、日常操作、人工编辑、发布、调度和 CI。
- [带注释的配置](../../examples/fani.toml) — 完整的配置字段和示例。
- [发布流程与资产约定](docs/release.md) — 安装验证、发布资产、可复现性及维护者审核关卡。
- [原生架构契约](docs/architecture/native-i18n.md) — 有效行为和权限边界。
- [ADR-0001](docs/architecture/adr-0001-native-single-authority.md) — fani 为何使用原生 Rust 和单一 SQLite 权威源。

历史设计文档在[文档索引](docs/README.md#历史记录)中被标记为已取代，不作为使用指南。
