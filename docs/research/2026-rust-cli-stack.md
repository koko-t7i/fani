# 2026 Rust CLI 技术栈调研：面向 fani 的采用与拒绝建议

- **研究截止日 / 访问日期：** 2026-08-29
- **独立研究归属：** 子 Agent `01a04eca-0846-7282-a5c4-00ceb33e96ab` 于 2026-08-29 完成资料检索和候选栈比较；主实现 Agent 将其结论与兼容性审计、流程审计合并到重构设计。本文保留子 Agent 的独立发现，最终采用偏差记录在 [`../architecture/rust-sqlite-rewrite.md`](../architecture/rust-sqlite-rewrite.md)。
- **证据范围：** 仅使用可访问的官方项目文档、docs.rs/crates.io crate 页面、官方发布页、Rust 标准库文档、systemd 手册和 SQLite 官方文档。
- **“趋势”的口径：** 本文不使用下载量或社区印象推断流行度；只根据截至截止日可核验的版本发布、官方 API 定位和维护文档判断技术方向。
- **fani 调研输入基线：** Python 0.1 实现以 TOML 供人配置、JSON 作为 skill/任务/报告边界；用有界线程并发调用外部 Agent CLI；通过独立进程组执行 Agent；由 systemd oneshot 定时运行；用 Git plumbing 和临时 index 发布，不切换 HEAD 或污染真实 index。[F1][F2][F3][F4][F5][F6]

## 1. 结论摘要

| 领域 | 对 fani 的结论 | 采用强度 |
| --- | --- | --- |
| CLI | `clap` derive 为主，builder 只用于确有动态性的局部 | 采用 |
| 并发/子进程 | `tokio` 管 Agent/skill/Git 子进程、超时与信号；阻塞 SQLite 放专用线程或 `spawn_blocking` | 采用，但限制边界 |
| SQLite | 只有在需要可查询的任务账本、崩溃恢复和幂等 claim 时才引入；届时选 `rusqlite`，不选 `sqlx` | 条件采用 / 拒绝 SQLx |
| 配置与协议 | `serde` + `serde_json` + `toml`；配置严格校验，协议使用带版本的类型化 envelope | 采用 |
| 可观测性 | `tracing` + `tracing-subscriber`；日志只到 stderr/journald，stdout 保留给协议数据 | 采用 |
| 错误 | `thiserror` 定义领域错误，`anyhow` 只用于 CLI/任务顶层补充上下文 | 采用组合 |
| 测试 | `assert_cmd` 做黑盒 CLI/退出码/stdio；`tempfile` 隔离仓库、HOME、状态库和输出文件 | 采用 |
| 发布 | `cargo-dist` 生成 release CI 与安装产物；初期不把 `cross` 设为默认发布主干 | 采用 / 暂拒 cross 主干 |
| 供应链 | `cargo-deny` 做许可证、来源、重复版本与 advisory 策略；`cargo-audit` 独立扫描最终 `Cargo.lock` | 两者采用 |
| 无人值守进程 | 每个 Agent 建独立 Unix process group；超时 TERM→宽限→KILL→wait；systemd 显式 `KillMode=control-group` | 必须 |
| SQLite 可靠性 | 本地文件系统、显式事务/忙等待/FK/同步级别、在线备份、定期检查；默认不启用 WAL | 必须（若采用 SQLite） |

上述组合不是“异步越多越好”：Tokio 官方把自己定位为异步 I/O runtime，并明确指出长时间不抵达 `.await` 的代码会阻塞 core thread，阻塞工作应交给 `spawn_blocking`，CPU 密集型工作则宜用受限的独立线程池。[S3] fani 的高并发对象是长时外部进程而不是数据库查询，[F2] 因而 Tokio 适合管进程生命周期，而不构成选择异步 SQLite 层的理由。

## 2. 截止日版本与发布信号

下表来自 crates.io 官方 API/版本页及对应官方发布页；“更新时间”是 crate 页面记录的版本更新时间，不等同于本文访问日期。

| crate | 截止日可见版本 | 版本/页面日期 | 可核验信号 |
| --- | ---: | --- | --- |
| clap | 4.6.6 | 2026-08-06 | 4.x 持续发布；官方同时维护 derive 与 builder 教程。[S1][S2] |
| tokio | 1.53.1 | 2026-07-20 | 1.x 稳定线继续发布；process、sync、timeout 均在同一 runtime 下提供。[S3][S4] |
| rusqlite | 0.40.2 | 2026-08-08 | 同步 SQLite binding 仍活跃发布。[S5] |
| sqlx | 0.9.0 | 2026-05-21 | 异步多数据库 toolkit 进入 0.9；支持 Tokio/async-std runtime 和异步连接池。[S6] |
| serde | 1.0.229 | 2026-07-18 | 1.0 稳定序列化接口持续发布。[S7][S8] |
| toml | 1.1.4+spec-1.1.0 | 2026-07-28 | crate 版本明确对应 TOML spec 1.1.0。[S9] |
| tracing | 0.1.44 | 2025-12-18 | 0.1 API 继续作为结构化诊断基础；生态通过 Subscriber 消费 span/event。[S10] |
| thiserror | 2.0.20 | 2026-08-08 | 2.x derive 线活跃，且宏不进入公共 API。[S11] |
| anyhow | 1.0.104 | 2026-07-18 | 1.x 应用错误封装持续发布。[S12] |
| assert_cmd | 2.2.2 | 2026-05-11 | 继续提供二进制发现、stdin、timeout、退出码/stdout/stderr 断言。[S13] |
| tempfile | 3.27.0 | 2026-03-11 | 3.x 活跃；官方继续强调命名临时文件与清理器的安全边界。[S14] |
| cargo-dist | 0.32.0 | 2026-05-22 | 官方 release 工作流仍活跃，可生成 CI、安装器和发布产物。[S15][S16] |
| cross | 0.2.5 | 2023-02-04 | crates.io 稳定版及官方 latest release 均停在 0.2.5；不据此断言项目停维，但不宜把它作为唯一发布链路。[S17][S18] |
| cargo-audit | 0.22.2 | 2026-06-05 | RustSec 的 `Cargo.lock` 漏洞扫描工具仍活跃发布。[S19] |
| cargo-deny | 0.20.2 | 2026-07-09 | advisory、license、ban、source 策略工具仍活跃发布。[S20][S21] |

## 3. 分项取舍

### 3.1 clap：采用 derive-first

`clap` 官方将 derive 和 builder 都列为一等入口；derive 适合把静态命令结构映射成 Rust 类型，builder 适合运行时组装命令。[S2] fani 的公开命令目前只有 `status`、`sync`、`doctor`，参数形状稳定，[F1] 因而建议：

- 顶层使用 `#[derive(Parser)]` / `#[derive(Subcommand)]`，让参数、帮助和类型验证保持同源。[S2]
- `AgentConfig.cmd` 必须继续是“字符串数组”而不是 shell 字符串；它是传给外部程序的 argv，不应被 clap 再解释。[F5][S30]
- 只在未来插件命令确需运行时发现时使用 builder；当前为三个固定子命令引入动态 builder 没有官方 API 上的必要性。[S2][F1]
- 拒绝自写 argv 解析。clap 已提供 derive/builder、帮助、子命令与参数校验，而 fani 的无人值守退出码契约需要稳定且可测试的解析行为。[S2][F1][S13]

### 3.2 Tokio 与同步并发：采用 Tokio 管 I/O，不让它吞掉阻塞工作

Tokio 官方说明：任务只能在 `.await` 点让出 core thread；阻塞代码应使用 `spawn_blocking`，CPU 密集任务宜用独立受限线程池。[S3] Tokio 的 process API 支持异步等待、`kill_on_drop` 和 Unix process group 配置。[S4][S31] 这与 fani 一次运行并发多个长时 Agent CLI、每个调用有 timeout/retry 的形态直接吻合。[F2][F5]

建议边界：

1. **Tokio runtime 负责：** Agent CLI、i18n skill、Git 子进程，timeout，信号升级，有限并发 semaphore，异步读取 stdout/stderr。[S3][S4]
2. **普通同步代码负责：** clap 解析、serde/TOML 解码、纯状态机判断、短小文件操作；不要为了“全 async”把纯计算包装成异步。[S3][S7][S9]
3. **阻塞 SQLite 负责：** 若采用 rusqlite，用单独 DB actor 线程串行持有连接，或把短事务放入 `spawn_blocking`；不得在 Tokio core thread 上执行可能等待锁或磁盘的事务。[S3][S5][S25]
4. **并发上限继续来自每 Agent 配置，** 不使用无限 `join_all`；fani 当前 `concurrency` 是明确安全阀。[F2][F5]

若 Rust 版本只需顺序调用 skill/Git 且 Agent 并发很低，标准线程也能实现；但 fani 已有每 Agent 有界并发、进程 timeout、进程树清理和未来流式 stderr 的组合，[F2][F3] 因此 Tokio 在这里减少的是生命周期状态机复杂度，而不是提高 SQLite 吞吐。[S3][S4]

### 3.3 rusqlite 与 sqlx：默认 rusqlite，暂拒 sqlx

`rusqlite` 是 SQLite 的同步 Rust binding，API 围绕 `Connection`、prepared statement、transaction/savepoint 展开。[S5] `sqlx` 是支持 SQLite/PostgreSQL/MySQL 的异步 toolkit，提供异步连接池、runtime 选择、迁移和查询宏。[S6] 两者都能用 SQLite，但产品边界不同。

对 fani 的建议：

- **当前阶段不为了“技术栈完整”强行引入数据库。** 当前 `state.json` 由 skill 拥有，fani 已通过排他锁、备份、整库重翻译 guard 和 Git 同提交保护它。[F4][F6] 只有当 Rust 重写要接管“任务 claim、attempt、run、event、幂等恢复”这些可查询状态时，SQLite 才提供明确收益。
- **引入时选 `rusqlite`。** fani 是单机 systemd oneshot、单写者、数据库种类固定，查询规模小；rusqlite 的同步连接/事务模型与“单 DB actor”直接匹配。[S5][S24][S25][F4]
- **暂拒 `sqlx`。** 其异步连接池和多数据库抽象是真实能力，[S6] 但 fani 不需要多后端，SQLite 在 WAL 下也仍只有一个 writer。[S24] 把 SQLx runtime、pool 和查询构建链带进来不会改变这个 SQLite 写并发上限。[S6][S24]
- **重新评估 SQLx 的触发条件：** 未来若状态库迁到 PostgreSQL、同一服务需要大量并发网络数据库请求，或团队明确依赖 SQLx 的查询宏/迁移工作流，再采用。[S6]
- **优先启用 rusqlite 的 `bundled` 路径并做运行时版本检查。** 这样发布物不依赖目标机恰好安装何种 SQLite；同时必须确认实际 `sqlite_version()` 不低于 3.51.3，因为 SQLite 官方披露 WAL-reset 罕见损坏缺陷存在于 3.7.0—3.51.2，并在 3.51.3 修复；截止日 SQLite 最新发布为 3.53.4（2026-07-24）。[S24][S29]

### 3.4 serde / JSON / TOML：采用严格、版本化边界

Serde 官方模型是由数据结构实现 `Serialize`/`Deserialize`，数据格式 crate 负责具体编码；TOML crate 提供 TOML 到 Rust 类型的序列化/反序列化。[S7][S9] fani 已明确区分“人写 TOML、机器写 JSON”，skill 的所有结构化调用也要求 JSON。[F3][F5]

建议：

- 配置：`serde` + `toml`，根配置和关键子表使用 `#[serde(deny_unknown_fields)]`，保留当前“任何配置错误发生在模型调用和仓库写入之前”的契约。[F5][S7][S9]
- 协议：`serde_json` + 明确 struct/enum，不在核心逻辑中流转无约束 `Value`。每个跨进程 envelope 带 `schema_version`、`kind`、稳定 ID 和必填字段；未知版本应报协议错误，而不是猜测。[S7][F2][F3]
- stdout：skill/review 的 stdout 只允许一个 JSON 文档；诊断走 stderr。当前 skill wrapper 已把“非 JSON stdout”视为致命边界错误，应保留。[F3]
- Agent 文本：翻译 Agent 的 stdout 本质仍是不可信文本；只在协议明确要求时解 JSON envelope，并继续校验 `chunk_id` 与字段类型。[F2][S7]
- 写结果：先在目标目录创建临时文件，写完并关闭后再原子持久化/rename；`tempfile` 的 `NamedTempFile`/persist API适合管理此生命周期，但官方提醒共享临时目录和外部临时清理器会破坏路径安全假设，因此优先使用目标仓库内、权限受控的状态目录。[S14][F2]

### 3.5 tracing：采用，严格分离日志与协议通道

`tracing` 官方将诊断表示为结构化 `event` 和具有持续时间/上下文的 `span`，由 Subscriber 收集和处理；它比拼字符串日志更适合异步系统中跨任务关联因果关系。[S10]

fani 建议字段：`run_id`、`repo`、`lang`、`stage`、`task_id`、`agent`、`attempt`、`pid`、`duration_ms`、`exit_code`、`error_code`。Agent prompt、翻译正文、环境变量和凭据不得进入 event 字段；这是因为 fani 的 Agent 输入含完整文档块，而凭据来自 systemd EnvironmentFile。[F2][F6]

输出建议：交互终端用紧凑文本 formatter；systemd 下写 stderr，由 journald 采集；可选 JSON formatter 供机器处理。绝不能把 tracing subscriber 指向 stdout，因为 skill/Agent 的 stdout 是 JSON或翻译协议通道。[S10][F2][F3]

### 3.6 thiserror / anyhow：组合采用，不二选一

`thiserror` 生成标准 `std::error::Error` 实现，并明确不出现在公共 API 中；改回手写实现也不构成 breaking change。[S11] `anyhow` 面向应用级错误传播，可用 `Context`/`with_context` 给底层错误补充“正在执行哪个高层步骤”，并支持可选 backtrace。[S12]

建议：

- 库/领域层用 `thiserror`：`ConfigError`、`ProtocolError`、`ProcessError`、`SkillError`、`GitError`、`DbError`，保留稳定分类和 source chain。[S11]
- `main`、单次 repo/lang run、一次 task attempt 的顶层用 `anyhow::Result` 与 `.context(...)`，添加路径、stage、命令名和 task ID。[S12]
- 对外退出码不能直接由 anyhow 字符串决定。继续保留 fani 的 0/1/2/3 语义映射，并由领域错误/运行结果类型决定 exit code。[F1]
- 报告 JSON 存稳定机器码与结构化字段；anyhow 的展示链只进入人类报告和 tracing，避免把措辞当 API。[S12][F1]

### 3.7 assert_cmd / tempfile：采用端到端黑盒测试

`assert_cmd` 官方 API直接覆盖二进制发现、stdin、环境、cwd、timeout、退出码、stdout 和 stderr 断言。[S13] `tempfile` 提供临时目录和命名临时文件，同时说明命名路径在 Unix 共享临时目录/清理器下存在额外安全问题。[S14]

测试矩阵应至少覆盖：

- `status/sync/doctor` 的 0/1/2/3 退出码和 stdout/stderr 分离。[S13][F1]
- fake Agent：成功、空输出、非零退出、无效 review JSON、超时、忽略 TERM、派生孙进程、输出文件模式。[S13][F2]
- fake skill：合法 JSON、stdout 污染、exit 2、exit 3、部分写入、超时。[F3][S13]
- temp Git repo：脏 working tree、独立真实 index、并发移动目标 ref、只提交 allowlist、HEAD 不变。[F4][S14]
- temp SQLite：进程被 KILL、事务中断、busy timeout、重复执行幂等、备份恢复和版本下限检查。[S23][S25][S26][S28][S29]
- 所有测试显式设置临时 `HOME`、配置目录、repo、state、report 和 DB 路径，避免读取开发机 Agent 凭据或真实 Git 配置。[S13][S14][F6]

### 3.8 cargo-dist / cross：采用 cargo-dist，暂不以 cross 为主干

cargo-dist 官方文档将其定位为开源项目的 release orchestration，可生成 CI、构建归档、校验和与安装器；0.32.0 于 2026-05-22 发布。[S15][S16] cross 官方定位是借助容器提供“zero setup”交叉编译和交叉测试，但 crates.io 与 latest release 的稳定版均为 0.2.5（2023-02-04）。[S17][S18]

建议：

- 用 cargo-dist 生成 GitHub Releases 流程，先发布 Linux `x86_64-unknown-linux-gnu`；若实际用户需要，再加 musl、aarch64 Linux 和 macOS。[S15]
- systemd unit、示例配置和 shell completion 作为附加资产或安装后说明；发布流程必须从 tag 构建并生成 checksum。[S15]
- 初期使用各目标的原生 CI runner 验证产物，尤其是涉及 SQLite bundled、Git、进程组和 systemd 行为的 Linux 目标。[S15][S24][S30]
- 不把 cross 设为唯一构建/测试入口。其能力仍适合补充 Linux target coverage，[S17] 但当前稳定发布间隔明显长于 cargo-dist；这是降低关键发布链单点风险的依据，不是对 cross 项目维护状态的推断。[S16][S18]

### 3.9 cargo-audit / cargo-deny：两层采用

cargo-audit 使用 RustSec Advisory Database 扫描 `Cargo.lock` 的已知漏洞。[S19] cargo-deny 可统一检查 advisories、licenses、crate bans/重复版本和 sources。[S20][S21]

建议：

- 每个 PR：`cargo deny check`，策略文件明确允许许可证、Git/registry 来源、禁用 crate 和重复依赖阈值。[S20][S21]
- 每个 PR及定时任务：`cargo audit --deny warnings` 扫最终 `Cargo.lock`；定时任务可在无代码变更时发现新披露 advisory。[S19]
- deny 的 advisory 检查与 audit 有重叠，但保留独立 audit 可直接验证 RustSec/lockfile 路径；两者输出分别作为“供应链策略”和“已知漏洞”证据，不把任一成功误解为完整安全证明。[S19][S20]
- 临时 ignore 必须写 advisory ID、原因和到期日；该治理要求属于 fani 的策略选择，检查器本身只提供 advisory/policy 能力。[S19][S20]

## 4. 无人值守子进程可靠性基线

Rust 标准库 `Command` 提供显式 argv、cwd、环境和 stdio 配置；官方还指出修改子进程环境时最好使用绝对程序路径或显式 PATH，避免平台搜索差异。[S30] Unix `CommandExt::process_group(0)` 可令子进程以自身 PID 建立新 PGID，进程组决定信号投递范围。[S32] systemd 的 `KillMode=control-group` 会在停止 unit 时终止该 cgroup 中所有剩余进程；`process` 模式只杀主进程且官方标为不推荐。[S33]

### 4.1 启动

- 配置保存 argv 数组，禁止拼 shell 命令；只有明确需要 shell 语法的适配器才调用 shell，并将其视为单独受审计类型。[S30][F5]
- `doctor` 解析/验证可执行文件，运行时优先使用绝对路径；给 Agent 传最小环境白名单，Git/skill 也显式控制 cwd。[S30][F3][F5]
- stdin/stdout/stderr 均显式设置。Agent prompt 写完立即关闭 stdin；stdout 是返回协议，stderr 是有上限的诊断缓冲。[F2][S30]
- Unix 每个 Agent 调用设置 `process_group(0)`；不要只记录 direct child PID。[S32]

### 4.2 等待、超时与取消

- 用 Tokio `select!`/timeout 同时等待 child、deadline 和服务取消信号；Tokio process API支持异步 child 管理。[S4][S31]
- deadline 到达后向整个 PGID 发 TERM，等待短宽限期，再向整个 PGID 发 KILL；最后总是 `wait`/reap。仅调用 direct child 的 kill 无法保证孙进程退出，进程组正是 Unix 用于信号分发的机制。[S32]
- 不依赖 drop 作为正常清理路径。Tokio `Child` 官方说明默认 drop 不会终止子进程，需显式配置 `kill_on_drop`；即使启用它，也应把显式 TERM→KILL→wait 作为主路径，`kill_on_drop` 只作 panic/cancellation 兜底。[S31]
- stdout/stderr 必须并发排空或使用等价 `wait_with_output` 路径，避免子进程写满 pipe 后父进程只等待退出。标准库/ Tokio 都把 piped stdio 与等待作为显式 child 生命周期 API。[S4][S30][S31]
- 每个 attempt 使用新进程、新临时输出路径和新诊断缓冲；重试不能复用可能仍被孤儿进程持有的文件。[F2][S14]

### 4.3 systemd 契约

fani 当前 unit 已是 `Type=oneshot`、`TimeoutStartSec=2h`，并把 0/1/3 设为成功状态。[F6] systemd 文档说明 oneshot 默认不设启动超时，因此 fani 显式设置上限是必要的。[S34]

Rust 版本应继续：

- 显式 `KillMode=control-group`，即使这是常见默认值，也让“停止 unit 清理所有后代”成为可审查配置。[S33]
- 保留 `TimeoutStartSec` 大于单 Agent timeout、但小于不可接受的调度占用时间；fani 当前每 run 还有 `max_tasks` 上限。[F1][F5][F6][S34]
- 保留 `SuccessExitStatus=0 1 3`，让“需人工”和“部分完成”不是 systemd 基础设施故障。[F1][F6]
- 日志写 stderr/journald，报告文件原子替换；stdout 不承担长期日志。[S10][F1]

## 5. SQLite 可靠性基线（若 fani 引入 SQLite）

### 5.1 文件系统与版本

- DB、journal/WAL/SHM 必须位于同一台主机的本地可靠文件系统。SQLite 官方明确说 WAL 依赖共享内存，不能工作在 network filesystem 上。[S24]
- 截止日采用 SQLite 3.53.4 或至少 3.51.3。官方 WAL 文档记录 3.7.0—3.51.2 存在罕见 WAL-reset 损坏缺陷，3.51.3 修复，页面于 2026-08-25 更新；最新 change log 列出 3.53.4（2026-07-24）。[S24][S29]
- 使用 rusqlite bundled 时仍在启动/doctor 中查询并记录 `sqlite_version()`；“启用了 bundled”不能替代对实际发布物的验证。[S5][S29]

### 5.2 默认 journal 模式：先用 rollback journal，不默认 WAL

WAL 的官方优点是读写可并发、通常更快；限制是同一时刻仍只有一个 writer、需要 checkpoint、长读事务会阻碍 checkpoint，而且不支持网络文件系统。[S24] fani 由单实例锁和 systemd oneshot 驱动，主要写入是短状态事务，[F4][F6] 因此默认选择 `journal_mode=DELETE`（或保持 SQLite 默认 rollback journal）更简单；只有观测到并发 reader 被 writer 阻塞且数据库确认在本地文件系统时才启用 WAL。[S24][S27]

若启用 WAL：

- 监控 WAL 大小和 checkpoint 结果；不要假设自动 checkpoint 永远能推进，长 reader 会阻止它越过 reader end mark。[S24]
- 备份/搬运不能只复制 `.db` 而忽略 `-wal`；SQLite 官方说明 WAL 文件是数据库持久状态的一部分，最后连接未正常关闭时会保留并在下次打开时恢复处理。[S27]
- 任何维护工具都必须打开数据库走 SQLite API，不手工删除 `-wal`/`-shm`。[S27]

### 5.3 连接初始化

每个连接打开后、事务开始前统一执行并验证：

- `PRAGMA foreign_keys=ON`；官方说明它只能在无 pending transaction/savepoint 时切换，因此不能在事务中“顺手设置”。[S26]
- 设置 busy timeout；SQLite 官方提供 `PRAGMA busy_timeout`/busy handler，避免短暂锁竞争立即变成失败。[S26]
- `PRAGMA synchronous=FULL` 作为无人值守状态账本默认值；官方定义 FULL 会在继续前同步关键内容，EXTRA 在 rollback DELETE 模式对 journal unlink 后的目录再同步，代价更高但耐近邻掉电更强。[S26]
- 记录实际 `journal_mode`、`synchronous`、`foreign_keys`、SQLite 版本到启动 tracing event，避免部署配置漂移。[S10][S26]

### 5.4 事务、单写者与幂等

SQLite 官方说明所有读写都发生在事务中；DEFERRED 事务从读升级为写时，若其他连接已写，会返回 `SQLITE_BUSY`，而 IMMEDIATE 会在 BEGIN 时就尝试启动写事务。[S25]

fani 的 task claim/run finalization 建议用短 `BEGIN IMMEDIATE` 事务：

1. 原子选择并 claim 待执行 task；
2. 提交后再启动 Agent，绝不在远程模型调用期间持有 DB 事务；
3. attempt 完成后用另一短事务按 `(task_id, attempt_no)` 写结果；
4. 用 UNIQUE/主键约束保证重复 timer、崩溃重放和 retry 不产生双记录；
5. busy timeout 后仍失败则返回稳定 DB_BUSY 机器码，不无限重试。

这里选择 IMMEDIATE 的依据是它把写锁竞争提前到事务开头，[S25] 而“远程调用不持事务”和幂等键来自 fani 已有长时 Agent、retry、排他运行和无人值守恢复要求。[F2][F4][F5]

### 5.5 崩溃恢复、检查与备份

- SQLite rollback journal 在异常中断后会留下 hot journal，下次打开时用它恢复未完成事务；不得把 journal 当垃圾文件删除。[S27]
- 定期运行 `PRAGMA quick_check` 作为轻量检查，并在维护窗口运行 `PRAGMA integrity_check`；外键一致性另跑 `PRAGMA foreign_key_check`，因为官方 PRAGMA 文档把它们定义为不同检查。[S26]
- 在线备份使用 SQLite Online Backup API或 `VACUUM INTO`，不在数据库运行时直接 `cp` 单个主文件。官方说明 Online Backup API 可增量读取，完成后目标是源数据库开始复制时的 snapshot。[S28]
- 备份恢复测试必须实际打开副本、检查 schema/user_version、运行 integrity/foreign-key checks，并执行一次只读 `fani status`；只验证备份文件存在不构成可恢复性证据。[S26][S28][F1]
- schema migration 在独占维护事务中执行，使用 `PRAGMA user_version` 或迁移表记录；升级失败应回滚并让 systemd run 以基础设施错误退出，不能继续调用 Agent。[S25][S26][F1]

## 6. 面向 fani 的建议依赖轮廓

首个 Rust 版本建议的直接依赖边界：

- 必选：`clap`、`tokio`（仅所需 feature）、`serde`、`serde_json`、`toml`、`tracing`、`tracing-subscriber`、`thiserror`、`anyhow`。[S2][S3][S7][S9][S10][S11][S12]
- dev-dependencies：`assert_cmd`、`tempfile`。[S13][S14]
- 条件依赖：只有确定由 fani 接管持久任务账本时加入 `rusqlite`，优先 bundled；暂不加入 `sqlx`。[S5][S6][F3][F4]
- 构建/发布工具：`cargo-dist`；`cargo-deny` 与 `cargo-audit` 进入 CI/定时任务；`cross` 仅作为可选目标验证工具。[S15][S17][S19][S20]

实现顺序建议：先复刻现有 TOML/JSON/退出码/Git plumbing 契约，再替换子进程调度器，最后才评估 SQLite。这个顺序以 fani 当前安全边界为依据：模型无文件权限、skill 是 JSON 边界、发布只允许指定路径、状态丢失有 guard。[F1][F2][F3][F4] 若先引入数据库或异步抽象而尚未复刻这些边界，将无法用现有端到端行为判断 Rust 重写是否保持安全语义。[S13][F1]

## 7. 来源登记

所有在线来源访问日期均为 **2026-08-29**。无官方页面修订日期时标记为“未标注”；crate 日期来自 crates.io 版本记录或官方 release。

### 官方在线来源

- **[S1]** crates.io, `clap` 最新可见版本 4.6.6；版本日期 2026-08-06。<https://crates.io/api/v1/crates/clap>
- **[S2]** docs.rs, `clap` 4.6.6 crate 文档（derive/builder 导航）；页面构建日期 2026-08-11。<https://docs.rs/clap/4.6.6/clap/>
- **[S3]** docs.rs, `tokio` 1.53.1 crate 文档，“CPU-bound tasks and blocking code”；版本日期 2026-07-20。<https://docs.rs/tokio/1.53.1/tokio/index.html#cpu-bound-tasks-and-blocking-code>
- **[S4]** docs.rs, `tokio::process::Command` 1.53.1；版本日期 2026-07-20。<https://docs.rs/tokio/1.53.1/tokio/process/struct.Command.html>
- **[S5]** docs.rs / crates.io, `rusqlite` 0.40.2；版本日期 2026-08-08。<https://docs.rs/rusqlite/0.40.2/rusqlite/>；<https://crates.io/api/v1/crates/rusqlite>
- **[S6]** docs.rs / crates.io, `sqlx` 0.9.0；版本日期 2026-05-21。<https://docs.rs/sqlx/0.9.0/sqlx/>；<https://crates.io/api/v1/crates/sqlx>
- **[S7]** Serde 官方文档，Overview；页面日期未标注。<https://serde.rs/>
- **[S8]** crates.io / 官方 release, `serde` 1.0.229；版本日期 2026-07-18。<https://crates.io/api/v1/crates/serde>；<https://github.com/serde-rs/serde/releases/tag/v1.0.229>
- **[S9]** docs.rs / crates.io, `toml` 1.1.4+spec-1.1.0；版本日期 2026-07-28。<https://docs.rs/toml/1.1.4+spec-1.1.0/toml/>；<https://crates.io/api/v1/crates/toml>
- **[S10]** docs.rs, `tracing` 0.1.44；版本日期 2025-12-18。<https://docs.rs/tracing/0.1.44/tracing/>
- **[S11]** docs.rs / 官方 release, `thiserror` 2.0.20；版本日期 2026-08-08。<https://docs.rs/thiserror/2.0.20/thiserror/>；<https://github.com/dtolnay/thiserror/releases/tag/2.0.20>
- **[S12]** docs.rs / 官方 release, `anyhow` 1.0.104；版本日期 2026-07-18。<https://docs.rs/anyhow/1.0.104/anyhow/>；<https://github.com/dtolnay/anyhow/releases/tag/1.0.104>
- **[S13]** docs.rs / crates.io, `assert_cmd` 2.2.2；版本日期 2026-05-11。<https://docs.rs/assert_cmd/2.2.2/assert_cmd/>；<https://crates.io/api/v1/crates/assert_cmd>
- **[S14]** docs.rs / crates.io, `tempfile` 3.27.0；版本日期 2026-03-11。<https://docs.rs/tempfile/3.27.0/tempfile/>；<https://crates.io/api/v1/crates/tempfile>
- **[S15]** cargo-dist 官方文档；页面日期未标注。<https://axodotdev.github.io/cargo-dist/book/>
- **[S16]** cargo-dist 官方 release 0.32.0；发布日期 2026-05-22。<https://github.com/axodotdev/cargo-dist/releases/tag/v0.32.0>
- **[S17]** cross 官方仓库 README；页面日期未标注。<https://github.com/cross-rs/cross>
- **[S18]** cross 官方 release 0.2.5；发布日期 2023-02-04。<https://github.com/cross-rs/cross/releases/tag/v0.2.5>
- **[S19]** docs.rs / crates.io, `cargo-audit` 0.22.2；版本日期 2026-06-05。<https://docs.rs/cargo-audit/0.22.2/cargo_audit/>；<https://crates.io/api/v1/crates/cargo-audit>
- **[S20]** cargo-deny 官方文档；页面日期未标注。<https://embarkstudios.github.io/cargo-deny/>
- **[S21]** cargo-deny 官方 release 0.20.2；发布日期 2026-07-09。<https://github.com/EmbarkStudios/cargo-deny/releases/tag/0.20.2>
- **[S22]** SQLite 官方文档，Atomic Commit；页面最后更新 2026-04-21。<https://sqlite.org/atomiccommit.html>
- **[S23]** SQLite 官方文档，Transactions；页面最后更新 2026-02-18。<https://sqlite.org/lang_transaction.html>
- **[S24]** SQLite 官方文档，Write-Ahead Logging；页面最后更新 2026-08-25。<https://sqlite.org/wal.html>
- **[S25]** SQLite 官方文档，Transaction / DEFERRED, IMMEDIATE, EXCLUSIVE；页面最后更新 2026-02-18。<https://sqlite.org/lang_transaction.html#deferred_immediate_and_exclusive_transactions>
- **[S26]** SQLite 官方文档，PRAGMA；页面最后更新 2026-06-04。<https://sqlite.org/pragma.html>
- **[S27]** SQLite 官方文档，Temporary Files Used By SQLite；页面最后更新 2025-06-12。<https://sqlite.org/tempfiles.html>
- **[S28]** SQLite 官方文档，Online Backup API；页面最后更新 2025-11-13。<https://sqlite.org/backup.html>
- **[S29]** SQLite 官方 change log/download；3.53.4 发布日期 2026-07-24，download 页面日期 2026-07-31。<https://sqlite.org/changes.html>；<https://sqlite.org/download.html>
- **[S30]** Rust 标准库，`std::process::Command`；stable 页面日期未标注。<https://doc.rust-lang.org/stable/std/process/struct.Command.html>
- **[S31]** docs.rs, `tokio::process::Child` 1.53.1；版本日期 2026-07-20。<https://docs.rs/tokio/1.53.1/tokio/process/struct.Child.html>
- **[S32]** Rust 标准库 Unix `CommandExt::process_group`；stable 页面日期未标注。<https://doc.rust-lang.org/stable/std/os/unix/process/trait.CommandExt.html#tymethod.process_group>
- **[S33]** systemd 官方手册，`systemd.kill`；latest 页面日期未标注。<https://www.freedesktop.org/software/systemd/man/latest/systemd.kill.html>
- **[S34]** systemd 官方手册，`systemd.service`；latest 页面日期未标注。<https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html>

### fani 仓库内依据

- **[F1]** [`README.md`](../../README.md)：命令、退出码、运行阶段、JSON 报告、systemd 和 Git plumbing 目标。
- **[F2]** [`src/fani/adapters.py`](../../src/fani/adapters.py) 与 [`src/fani/dispatch.py`](../../src/fani/dispatch.py)：Agent stdin/stdout、输出文件、进程组、timeout/retry、有界并发和 JSON 结果。
- **[F3]** [`src/fani/skill.py`](../../src/fani/skill.py)：i18n skill subprocess、显式 state dir、JSON stdout 边界与超时。
- **[F4]** [`src/fani/gitout.py`](../../src/fani/gitout.py)：临时 index、`read-tree`/`write-tree`/`commit-tree`/`update-ref`、路径 allowlist。
- **[F5]** [`src/fani/config.py`](../../src/fani/config.py) 与 [`examples/fani.toml`](../../examples/fani.toml)：TOML 配置、argv 数组、并发、timeout、retry、routing 和 guard。
- **[F6]** [`src/fani/lock.py`](../../src/fani/lock.py) 与 [`systemd/fani.service`](../../systemd/fani.service)：单实例锁、oneshot、启动上限、退出码和 sandbox。
