# CodeArts2API (Rust)

把华为云 CodeArts 的模型接口包装成标准 **OpenAI Chat Completions API** 的本地代理。
Rust 重写版，行为与仓库根目录的 Python 版（`server.py`）保持一致。

底层走华为云 `SDK-HMAC-SHA256` 请求签名，AK/SK 从 `.env` 读取。

## 与 Python 版的关系

本目录是独立实现，**不依赖 Python**，可直接编译为单文件可执行程序：

| 项 | Python 版 | Rust 版 |
|---|---|---|
| 运行时 | Python 3.12 + Flask | 无（静态链接） |
| 启动 | `python server.py` | `target\release\codearts2api.exe serve` |
| 分发 | 需装依赖 | 单个 exe，约 6 MB |
| 行为 | 基准 | 逐项对齐（见下文验证章节） |

两版共用同一个 `.env` 和 `models-cache.json`，可以互换使用，但**不要同时启动**——都默认监听 8787。

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
| `CODEARTS_MAX_TOKENS_CAP` | 模型元数据缺失时 `max_tokens` 的兜底上限，默认 `65536` |
| `CODEARTS_BENEFIT_FALLBACK` | `1` 时即使免费网关拿不到模型也列出兜底清单 |
| `CODEARTS_MODEL_CACHE` | 模型缓存路径，默认与 `.env` 同目录 |

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

启动代理后浏览器打开 **http://127.0.0.1:8787/**，即可在网页里管理账号：

- **概览** —— 账号数、可用模型数、启用状态、每个账号的模型数量
- **账号** —— 添加 / 删除 / 启停 / 轮换密钥，改动即时写入 `accounts.json` 并重建账号池
- **模型** —— 合并后的模型列表，可按名称筛选，点「测试」直接发起一次真实请求验证
- **客户端接入** —— 一键查看 Base URL、API Key 与推荐模型

界面基于 [miuix-console](https://github.com/bmbxwbh/miuix-console)（Apache 2.0，零依赖纯 CSS/JS），
已连同静态资源一起嵌入二进制，单文件部署即可，无需额外的静态目录。

安全说明：

- 账号页只显示打码后的 AK（`HPUA****L2`），完整密钥不会回传到浏览器
- 编辑账号时密钥输入框留空表示不修改
- 服务默认只监听 `127.0.0.1`，WebUI 与外网隔离
- 修改账号会先校验再落盘；校验失败自动回滚，不会留下会让下次启动失败的配置

## 客户端接入

```text
Base URL : http://127.0.0.1:8787/v1
API Key  : codearts      （任意非空字符串）
Model    : deepseek-v4.1-flash / glm-5.3 / kimi-k3 / openpangu-2.0-pro ...
```

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

## 相对上游做的兼容处理

这几点是踩坑后补的，如果以后改代码请注意别退化：

1. **`max_tokens` 一律按模型上限裁剪。**
   上游对超限不返回 400，而是 **HTTP 200 + `error_code: InferHub.001001005.400`**。
   客户端会看到一个"成功但没有内容"的响应，报错信息毫无指向性。
   本代理会读 `models-cache.json` 里的 `max_tokens` 裁剪，并打印
   `[param] <model>: max_tokens 384000 -> 65536 (上限 65536)`。

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
