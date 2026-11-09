# RPI_SELF_REVIEW

## 本次设计

本次选择的范围是 provider 后台任务中的一次重复深拷贝：

- `crates/rpi-ai/src/providers/openai_completions.rs`
- `crates/rpi-ai/src/providers/openai_responses.rs`

基线中的 Anthropic provider 已经在 `stream_simple` 中利用借用的 `Context` 生成 owned 请求参数；两个 OpenAI provider 仍先执行 `ctx.clone()`，把完整模型历史带入 `tokio::spawn`，再仅为构建请求体遍历这份副本。

本次改动在 `stream_simple` 仍持有 `&Context` 时调用既有 `build_request`，将生成的 owned `serde_json::Value` 移交给后台任务。Responses API 的事件状态机还需要工具定义，因此只额外克隆 `ctx.tools`；不再克隆包含完整历史的 `Context`。没有改变请求字段、JSON 编码入口、HTTP 重试、取消信号、SSE 处理或事件生产顺序。

## 保留的契约

- 完整模型历史仍由调用方/loop 保持；provider 只生成本次请求的完整 wire body，没有删减历史。
- `transform_context`、自定义 `convert_to_llm`、projector 和 `convertToLlm` 的输入路径未改动；本次只发生在 provider 已收到的 `Context` 边界之后。
- OpenAI Completions 与 Responses 的现有请求构建逻辑、工具/图片/思维内容映射和字段均复用原函数。
- 重试仍复用同一个 `PreparedJsonRequest`；取消仍使用 producer 的 cancellation token；错误仍通过原有 terminal error 事件路径返回。
- Responses 的工具状态匹配仍使用原始 `ctx.tools` 的等价 owned 副本。
- 未改动持久化 JSONL、恢复记录、RPC 字段、hooks、工具完成顺序或文件 IO。
- 未新增公开 SDK 接口、依赖、unsafe 或配置开关。

## 验证

均使用 Windows 本地离线命令，并复用既有 `CARGO_TARGET_DIR`：

- `cargo fmt --all -- --check`：通过。
- `git diff --check`：通过（工作区已有的行尾转换提示不属于 diff 错误）。
- `cargo test -p rpi-ai --features providers --locked --offline --jobs 2 --lib openai_completions`：22 通过，0 失败。
- `cargo test -p rpi-ai --features providers --locked --offline --jobs 2 --lib openai_responses`：50 通过，0 失败。
- `cargo test -p rpi-ai --features providers --locked --offline --jobs 2 --lib anthropic`：90 通过，0 失败，用于确认相邻 provider 请求/重试/编码路径未受影响。
- `cargo test -p rpi-ai --features providers --locked --offline --jobs 2 --lib`：237 通过，0 失败。

首次未带 `--features providers` 的筛选命令只运行了核心库并筛选到 0 个 provider 测试，随后已用正确 feature 重跑；不将那次 0 测试结果计入上述回归。

## 性能结论与未解决风险

本次没有运行统一性能对照，也没有可报告的实测收益。设计上消除了 OpenAI provider 后台任务携带完整历史的深拷贝，并把同一次请求构建提前到已有借用上下文的边界；实际收益仍取决于历史大小、wire 构建成本、调度和网络，应由外部审查者统一测量。

剩余方向仍包括 agent loop 每轮 `AgentMessage` 到 provider messages 的复制、provider-specific wire 转换/JSON 编码，以及 harness 的同步 `StreamFn` 桥接。这些路径涉及自定义转换和第三方 Provider ready 时序，本次没有冒险改变。另一个局部权衡是请求体现在 spawn 前同步构建；该构建本来就必须发生，且不包含文件 IO，但取消只能在该短同步构建完成后进入后台请求路径。现有取消/重试回归通过，若后续证据显示极端超大历史下该同步段影响取消延迟，应单独评估可借用的增量 wire 构建，而不是回退到完整 Context 深拷贝。
