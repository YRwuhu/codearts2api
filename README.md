# CodeArts2API

把华为云 CodeArts 的模型接口包装成标准 **OpenAI Chat Completions API** 的本地代理。

Rust 实现，编译为单文件可执行程序，**不依赖 Python 或任何运行时**。

底层走华为云 `SDK-HMAC-SHA256` 请求签名。凭证有两种来源：
OAuth 登录（推荐，无需手填）或 `.env` / `accounts.json` 里的 AK/SK。

## 构建

```cmd
cargo build --release
```

或双击 `build.bat`。产物：`target\release\codearts2api.exe`

只会用标准库之外的 crates：axum / tokio / reqwest / serde / hmac / sha2 / chrono。

## 配置

### 单账号

使用仓库根目录的 `.env`（也可用 `CODEARTS_ENV_FILE` 指定路径）：

```ini
CODEARTS_AK=你的AccessKey
CODEARTS_SK=你的SecretKey
```

### 多账号

复制 `accounts.example.json` 为 `accounts.json`，填入多个账号：

```json
{
  "accounts": [
    { "id": "main",   "ak": "第一个AccessKey", "sk": "第一个SecretKey", "enabled": true },
    { "id": "backup", "ak": "第二个AccessKey", "sk": "第二个SecretKey", "enabled": true }
  ]
}
```

`accounts.json` 已加入 `.gitignore`，不会被提交。存在该文件时优先于 `.env` 的单账号配置。

工作方式：

- **模型合并去重**：所有账号的模型汇总到 `/v1/models`，同一模型在多个账号可用时只出现一次
- **按序故障切换**：请求按 `accounts.json` 中的顺序尝试；遇到以下情况自动换下一个账号
  - 并发会话数已达上限（`TM.00001041`）
  - 鉴权失败（`APIG.0301` 等）
  - 该账号没有此模型（`benefit not found` / `not registered`）
- **不切换的情况**：客户端参数错误、模型不存在等，直接返回，避免无意义地打满所有账号
- **独立缓存**：多账号时每个账号的模型缓存分开存放（`models-cache.<id>.json`）

查看账号状态：

```cmd
codearts2api.exe accounts
```

```text
账号                 AK                 状态       模型数
------------------------------------------------------------
main               HPUA****L2         enabled  7
backup             HPUA****X9         enabled  5

合并去重后共 9 个模型:
  GLM-5.2                      可用账号: main, backup
  deepseek-v4.1-flash          可用账号: main
```

### 环境变量

| 变量 | 说明 |
|---|---|
| `CODEARTS_AK` / `CODEARTS_SK` | 单账号模式的华为云访问密钥 |
| `CODEARTS_ACCOUNTS` | 账号文件路径，默认 `.env` 同目录的 `accounts.json` |
| `CODEARTS_PROXY_HOST` | 监听地址，默认 `127.0.0.1`（只本机可访问） |
| `CODEARTS_PROXY_PORT` | 监听端口，默认 `8787` |
| `CODEARTS_MAX_TOKENS_CAP` | 模型元数据缺失时 `max_tokens` 的兜底上限，默认 `131072` |
| `CODEARTS_CONCURRENCY` | 每账号上游流式会话上限，默认 `3` |
| `CODEARTS_DAILY_TOKEN_CAP` | 每账号每日 token 上限，`0` = 不限制 |
| `CODEARTS_TOOL_TURN_PLAIN` | 工具轮改用非流式上游，默认 `1`（开） |
| `CODEARTS_STREAM_DEGRADE` | 名额耗尽时降级非流式，默认 `0`（关） |
| `CODEARTS_BENEFIT_FALLBACK` | `1` 时即使免费网关拿不到模型也列出兜底清单 |
| `CODEARTS_MODEL_CACHE` | 模型缓存路径，默认与 `.env` 同目录 |

## 登录（推荐，无需 AK/SK）

打开面板「账号」页，点 **登录华为云**：浏览器会打开华为云授权页，
用手机号或扫码登录一次即可。凭证自动保存到 `auths/`，有效期 24 小时，
到期前自动续期，**不需要手动填写任何密钥**。

实现方式（逆向自官方 `huaweicloud.authentication` 扩展）：

1. 生成 ticket_id / secret / PKCE 校验码 / DPoP 密钥对（ES256, P-256）
2. 构造 `codearts.huaweicloud.com/portal/authorize` 授权链接
3. 登录后双通道取凭据：
   - 主通道：portal 回调 `127.0.0.1:{port}/oauth/callback` 拿到 code
   - 兜底：轮询 `snap-manager /v1/login/ticket`
4. 用 code + PKCE verifier + DPoP 证明换 STS 临时凭证
5. 凭证落盘 `auths/codearts-{user_id}.json`（0600 权限）

关键约束：

- **refresh_token 与 DPoP 私钥绑定**：续期必须复用登录时的那对密钥，
  所以私钥要随凭证一起保存，否则续期报 `InvalidDPoPHeader`
- **client_id 固定为 `codearts-agent`**：换值会被 STS 拒绝

也支持手动 AK/SK（面板「添加账号」），两种账号可以并存。

## 运行

双击 `start.bat`，或：

```cmd
target\release\codearts2api.exe serve
```

启动后：

```text
CodeArts OpenAI 兼容代理已启动: http://127.0.0.1:8787
OpenAI Base URL: http://127.0.0.1:8787/v1
可用模型: [...]
```

## 管理界面 (WebUI)

启动代理后浏览器打开 <http://127.0.0.1:8787/>，即可在网页里管理账号：

- **概览** —— 账号数、可用模型数、启用状态、每个账号的模型数量
- **账号** —— 添加 / 删除 / 启停 / 轮换密钥，改动即时写入 `accounts.json` 并重建账号池
- **模型** —— 合并后的模型列表，可按名称筛选，点「测试」直接发起一次真实请求验证
- **额度** —— 套餐 credit 总额 / 已用 / 剩余、分套餐明细、累计 token，附华为云控制台跳转
- **用量** —— 请求数、token、平均耗时、失败数，含趋势图、按模型汇总与最近请求明细
- **签到** —— 每日签到积分自动领取，含活动列表与领取状态
- **客户端接入** —— 一键查看 Base URL、API Key 与推荐模型

界面基于 [miuix-console](https://github.com/bmbxwbh/miuix-console)（Apache 2.0，零依赖纯 CSS/JS），
已连同静态资源一起嵌入二进制，单文件部署即可，无需额外的静态目录。

安全说明：

- 账号页只显示打码后的 AK（`HPUA****L2`），完整密钥不会回传到浏览器
- 编辑账号时密钥输入框留空表示不修改
- 服务默认只监听 `127.0.0.1`，WebUI 与外网隔离
- 修改账号会先校验再落盘；校验失败自动回滚，不会留下会让下次启动失败的配置

## 并发与会话

上游对**流式**请求有硬限制，实测（2026-10，单账号、无其他客户端干扰）：

| 场景 | 结果 |
|---|---|
| `stream=false` 并发 4 个 | 全部成功，基本不限 |
| `stream=true` 同时 4 个 | 第 4 个立即 `TM.00001041` |
| 连续 3 个流（各 1.5 秒）后 | 新流要等到 t+58s |
| 连续 3 个流（各 35 秒）后 | 新流要等到 t+68s |

两个数据点都落在「**启动后** 60 秒」，而不是「结束后 60 秒」，所以约束是：

> **每个账号每分钟最多启动 3 个流式会话。**

这是**速率**限制，不是单纯并发限制——前 3 个流早就结束了，第 4 个照样被拒。

### 为什么单个对话也会触发

agent 类客户端（ZCode / Claude Code / Codex）**一个回合会发很多次请求**：
每执行一次工具就要再来一轮。一次"读文件 + 改文件 + 汇报"就是 3-5 次请求，
直接把这 3 个名额吃光，后续请求必然 `TM.00001041`。

所以这不是"你开了太多对话"，而是一个回合内的多次请求把窗口占满了。

### 代理侧怎么处理

**1. 工具轮改用非流式上游（默认开）**

带工具定义、或历史里已有 `tool` 结果的请求，一律用 `stream=false` 打上游，
再在本地合成 SSE 返回给客户端。这类请求产出的是工具调用、不需要正文增量
渲染，省下来的名额留给真正需要流式的最终回复。

实测效果：12 轮连续工具调用，每轮约 1 秒，零拒绝（改之前是每 3 轮卡 60 秒）。

关掉它：`CODEARTS_TOOL_TURN_PLAIN=0`。

**2. 滑动窗口排队**

窗口内已有 3 个流式启动记录时，第 4 个流式请求会排队到最早那次滑出窗口
（最长 60 秒），而不是直接报错。多账号时优先选还有名额的账号。

**3. 撞上限时自动重试**

即使排队了，窗口边界抖动仍可能让某个请求被拒；此时最多重试 2 次
（每次退避 8 秒），不把 429 直接甩给客户端。

**4. 可选：满载时降级非流式**

`CODEARTS_STREAM_DEGRADE=1` 时，若所有账号的流式名额都用完，改用非流式
上游 + 本地合成 SSE。**默认关闭**——降级会丢掉工具调用的增量帧，agent
类客户端体验明显变差。

### 想彻底不卡：加账号

每个账号有独立的 3 次/分钟额度。在「账号」页登录第二个华为云账号，
配额直接翻倍，模型列表自动合并去重，请求按可用名额分流。

### 会话钉号

同一对话固定路由到同一账号。这**不是**为了省名额（实测无效），而是因为
上游 **prompt cache 是账号级**的：同一账号再次看到相同前缀才命中。会话键
按以下顺序提取：

1. `X-Codearts-Chat-Id` / `X-Conversation-Id` / `X-Session-Id` 请求头
2. 请求体 `user` 字段
3. `prompt_cache_key` / `conversation_id` 字段
4. 首条 `user` 消息内容的哈希（多模态数组也支持）

钉号 30 分钟过期，账号被禁用或删除时自动释放。

### 健康与冷却

- 连续失败 3 次 → 指数退避（10 秒起，最长 5 分钟）
- 撞并发上限 → 冷却 60 秒
- **模型级冷却**：某账号缺某个模型时只冷却这一个模型 5 分钟，
  账号其他模型照常可用
- 冷却中的账号在选择时被跳过（除非全部都在冷却）

「账号」页的**健康**列可见：可用名额、当前并发、冷却剩余时间。

### 每日 token 上限（可选）

`CODEARTS_DAILY_TOKEN_CAP=2000000` 可给每账号设日 token 上限，本地时区
0 点归零。默认 `0` = 不限制。

## 额度与用量

### 额度（只读）

额度数据来自华为云 `snap-manager/v1/statistics/plugin` 接口，展示：

- 套餐 credit 总额 / 已消耗 / 剩余，以及各账号的消耗进度条
- 分套餐明细（基础套餐、赠送额度、按需付费）
- 累计消耗 token 数
- 各模型是否启用、是否豪华模型

后台每 **10 分钟**自动刷新一次；也可在页面上点「刷新额度」立即拉取。
额度用尽或需要续费时，页面提供直达华为云控制台的链接。

### 用量（本地统计）

代理在每次请求结束时记录一条用量：模型、账号、输入/输出 token、
推理 token、耗时、是否流式、成功与否。数据来源是上游返回的 `usage` 字段，
不是估算值。

- 内存中保留最近 20000 条
- 同时追加写入 `usage.jsonl`（与 `.env` 同目录，可用 `CODEARTS_USAGE_ARCHIVE` 覆盖）
- 用量页支持 24 小时 / 7 天 / 30 天切换，按模型和账号汇总

**关于缓存统计**：CodeArts 上游不返回 prompt cache 命中字段，
因此这里不提供"缓存命中率"——那只能是编造的数字。

### 每日签到

签到入口在 devcloud「活动福利」，实际接口位于 **snap-access 网关**：

```
GET  /v1/ops/delivery?channel=IDE            活动列表
POST /v1/ops/claim   {campaignId, channel, idempotentKey}
POST /v1/ops/confirm {campaignId}
```

**使用与聊天相同的 AK/SK 签名**，不需要浏览器、不需要额外登录。

实测发现两个关键细节，缺一个都会得到 `APIG.0301 verify ak sk signature fail`：

1. **必须携带 `X-Sdk-Content-Sha256` 头**，并用它的值作为 payload hash；
2. **参与签名的头集合必须与实际发送的完全一致**（华为云 SDK 会签名所有请求头）。

另一个容易踩的点：`channel` 要用 **`IDE`** 而不是 `WEB`。`WEB` 渠道走浏览器会话
鉴权，`IDE` 渠道才认 AK/SK 签名。

行为：

- 服务启动 15 秒后检查一次，之后每 30 分钟一轮
- 当天未签到时自动 claim + confirm，已签到则跳过
- 领取失败会记录原因（"尚未到达权益刷新时间" 视为今日已完成，不重复请求）
- 多账号时逐账号检查与领取

> 早期版本曾判断签到必须用浏览器（因为 devcloud 域名的接口确实会拦截非浏览器
> 请求）。那个结论是错的：正确做法是换到 snap-access 域名 + 补全签名头。

## 客户端接入

```text
Base URL : http://127.0.0.1:8787/v1
API Key  : codearts      （任意非空字符串）
Model    : deepseek-v4.1-flash / glm-5.3 / kimi-k3 / openpangu-2.0-pro ...
```

三种客户端协议指向同一个 Base URL：

| 客户端类型 | 端点 | 说明 |
|---|---|---|
| OpenAI 兼容 | `POST {Base}/chat/completions` | 最通用 |
| Codex / Responses | `POST {Base}/responses` | 需支持 Responses API 的客户端 |
| Claude Code / Anthropic SDK | `POST {Base}/messages` | 设置 `ANTHROPIC_BASE_URL=http://127.0.0.1:8787` |

## 子命令

```cmd
codearts2api.exe serve                 启动代理 + WebUI（默认）
codearts2api.exe accounts              列出账号、AK（打码）、模型数及模型归属
codearts2api.exe models                列出合并后的可用模型及通道/上下文/输出上限
codearts2api.exe selftest [MODEL...]   对运行中的代理做端到端自检（非流式 + 流式）
codearts2api.exe benefit status        查询签到状态
codearts2api.exe benefit claim         签到领取当日免费额度
codearts2api.exe benefit balance       查询免费额度余额
codearts2api.exe benefit models        查询免费模型列表
codearts2api.exe benefit claim backup  对指定账号签到（追加账号 id）
```

`selftest` 可用 `CODEARTS_SELFTEST_BASE` 指向其他地址（默认 `http://127.0.0.1:8787/v1`）。

## 接口

| 端点 | 说明 |
|---|---|
| `POST /v1/chat/completions` | OpenAI 格式聊天补全，支持 `stream` 流式与 `tools` |
| `POST /v1/responses` | OpenAI Responses API（Codex / 新版 SDK），含 `stream` 与 `function_call` |
| `POST /v1/messages` | Anthropic Messages API（Claude Code / Anthropic SDK） |
| `POST /v1/messages/count_tokens` | Anthropic token 估算（近似值） |
| `GET /v1/models` | 模型列表（含 context_window / max_tokens 等能力元数据） |
| `GET /v1/models/{id}` | 单模型详情 |
| `OPTIONS *` | CORS 预检 |
| `GET /api/status` | WebUI 概览数据（账号、模型、配置） |
| `GET /api/accounts` | 账号列表（密钥打码） |
| `POST /api/accounts` | 新增账号 |
| `PATCH /api/accounts/{id}` | 修改账号（启停 / 轮换密钥） |
| `DELETE /api/accounts/{id}` | 删除账号 |
| `POST /api/refresh` | 重新同步所有账号的模型 |
| `POST /api/test` | 用指定模型发起一次真实请求 |
| `GET /api/quota` | 各账号额度（credit / token / 分套餐）+ 控制台链接 |
| `POST /api/quota/refresh` | 立即刷新额度 |
| `GET /api/usage?hours=&bucket=` | 用量汇总与时间序列 |
| `GET /api/usage/recent?hours=` | 最近请求明细 |
| `POST /api/oauth/start` | 发起 OAuth 登录，返回授权链接 |
| `POST /api/oauth/poll` | 轮询登录结果 |
| `GET /oauth/callback` | OAuth 回调（portal 跳回） |
| `DELETE /api/oauth/account/{id}` | 移除 OAuth 账号 |
| `GET /api/checkin/status` | 各账号签到状态与活动列表 |

| `POST /api/checkin/run` | 立即对所有启用账号签到 |

| `GET /api/checkin/last` | 最近一次签到结果 |

## 相对上游做的兼容处理

这几点是踩坑后补的，如果以后改代码请注意别退化：

1. **`max_tokens` 按上游**真实**校验上限裁剪（131072），而不是模型元数据。**
   容易踩的坑：AgentCenter 的 `model_parameters.max_tokens` 恒为
   `65536`/`65535`，但那只是"建议值"。上游参数校验器实际接受
   **131072**，超出才报
   `RANGE_VALIDATOR: max_tokens -> max_tokens should be between 1 and 131072`。

   旧版直接信了元数据里的 65536，把客户端合法的 128k 请求砍成 64k，
   于是"官方说能输出 384k、反代只给 60k"。现在的规则是
   `cap = min(模型元数据, 131072)`，并且在元数据明显偏小时上抬到 131072
   （见 `params::UPSTREAM_MAX_OUTPUT_TOKENS`）。

   注意区分：**131072 是单次输出上限**；模型总上下文另见 `context_window`
   （deepseek-v4.1 / glm-5.3 / kimi-k3 均为 1M）。

2. **字段名归一化。**
   `max_tokens` / `max_completion_tokens` / `max_output_tokens` 统一收敛成
   `max_tokens`（上游不认后两个）；`reasoning`（Responses API 字段）直接丢弃。

3. **200 里的业务错误必须翻译。**
   JSON 和 SSE 两种形态都会出现 `error_code` / `error_msg`，代理会转成标准
   OpenAI `error` 对象，并按错误码后缀给出真实状态码（`.400`→400、`.429`→429），
   不再以 200 或误导性的 502 透传。

4. **`stream=false` 时上游也可能返回 SSE。**
   统一聚合为单个 `chat.completion` JSON。

5. **`finish_reason: "other"` 归一化为 `"stop"`。**
   `delta.content: null` 转成 `""`（有些 runtime 只认字符串）。

6. **两条签名通道。**
   AgentCenter 老套餐模型用基础签名；免费 benefit 模型必须额外签
   `maas_type: benefit` / `model-id` / `model-name` 三个头，否则上游报
   "model is not registered"。

## 三种对外协议

同一套上游通路，三种入站格式：

### OpenAI Chat Completions — `POST /v1/chat/completions`

原生格式，无损直通。`stream`、`tools`、`response_format` 全部支持。

### OpenAI Responses API — `POST /v1/responses`

供 Codex 与新版 OpenAI SDK 使用，做双向翻译：

| Responses | Chat Completions |
|---|---|
| `input`（字符串 / item 数组） | `messages` |
| `instructions` | 首条 `system` 消息 |
| `max_output_tokens` | `max_tokens` |
| `tools[].type=function` + `name` | `tools[].function.name` |
| `input_image` / `output_text` | `image_url` / `text` |
| `output`（`reasoning` / `message` / `function_call`） | `message.reasoning_content` / `.content` / `.tool_calls` |

流式输出是语义化事件序列：`response.created` → `response.in_progress` →
`response.output_item.added` → `response.output_text.delta`* →
`response.completed`，推理内容走 `response.reasoning_text.delta`，
工具调用走 `response.function_call_arguments.delta`。

`previous_response_id` 会作为会话键参与钉号（不参与上下文拼接——
本代理无状态，客户端需自带完整历史）。

### Anthropic Messages — `POST /v1/messages`

供 Claude Code / Anthropic SDK 使用：

- `system` 顶层字段 → `system` 消息
- `content` block 数组：`text` / `image` / `tool_use` / `tool_result`
- `tool_use.input`（对象） ↔ `tool_calls.function.arguments`（字符串）
- `tool_result`（user 消息里的 block）→ 独立的 `role: "tool"` 消息
- `stop_sequences` → `stop`；`tool_choice` 的 `any` → `required`
- `cache_control` 上游不支持，直接丢弃
- 推理内容以 `thinking` block 返回

流式事件严格按 Anthropic 顺序：`message_start` → `content_block_start` →
`content_block_delta`* → `content_block_stop` → `message_delta` → `message_stop`。

`POST /v1/messages/count_tokens` 用经验公式估算（中文约 1 字 1 token），
只够客户端做预算判断，不是精确 tokenizer。

### 空内容兜底

上游在 `max_tokens` 很小时可能把预算全花在 reasoning 上，`content` 返回空串
（`finish_reason: "length"`）。三种协议都做了兜底，不会静默返回空响应：

- Chat Completions：如实透传（`finish_reason=length`），由客户端判断
- Responses：`output_text` 用推理内容兜底，`output` 里两者都有
- Anthropic：补一个 `text` block（推理本来就在 `thinking` block 里）

### 保活

流式期间若上游 15 秒没有数据，代理会发 SSE 注释帧（OpenAI 下是
`: keepalive`，Anthropic 下是 `ping` 事件），防止中间代理把空闲连接掐掉。

## 已知限制

- **并发上限取决于账号套餐**，实测单账号套餐为 3。超出时上游返回
  `TM.00001041 并发会话数已达上限`；多账号模式下会自动切到下一个账号重试。
- **免费 benefit 通道需账号开通**。未开通时 `benefit-gateway-config` 返回
  `enabled:false`，签到/余额接口报 `invalid channel, group not found`。
  默认不把这类必然失败的模型列进 `/v1/models`。
- 仅绑定 `127.0.0.1`。若改 `CODEARTS_PROXY_HOST=0.0.0.0`，同网段任何人都能
  消耗你的额度，慎用。

## 测试

```cmd
cargo test                 12 个单元测试：签名规范、SSE 聚合、参数裁剪、错误映射
cargo run -- models        用真实凭证验证签名与模型同步
cargo run -- selftest      对运行中的实例做端到端验证
```

## 免责声明

本项目为独立技术研究作品，与华为云无任何合作或授权关系。
使用本工具产生的账号封禁、额度消耗、数据风险等后果由使用者自行承担。
