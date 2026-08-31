# 兼容性基准（已取代）

本文档描述了实验性 fani 0.2 外部技能/Python 兼容性边界。fani 尚未发布，因此 0.3 原生引擎特意不提供兼容性迁移或回滚协议。

本文档仅作为历史背景保留，不得用作实施或运维指南。已接受的替代决策为 [`ADR-0001`](adr-0001-native-single-authority.md)，当前生效的架构和产品约定见 [`native-i18n.md`](native-i18n.md)。运行时代码不得导入旧的 JSON/JSONL 状态、创建与 Python 兼容的锁、调用外部 i18n 技能，也不得依赖 Python/`uv`。
