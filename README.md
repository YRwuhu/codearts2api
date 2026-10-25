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
cd rust
cargo build --release
```

或双击 `build.bat`。产物：`target\release\codearts2api.exe`

只会用标准库之外的 crates：axum / tokio / reqwest / serde / hmac / sha2 / chrono。

## 配置

使用仓库根目录的 `.env`（也可用 `CODEARTS_ENV_FILE` 指定路径）：

```ini
CODEARTS_AK=你的AccessKey
CODEARTS_SK=你的SecretKey

# 可选
CODEARTS_PROXY_HOST=127.0.0.1
CODEARTS_PROXY_PORT=8787
CODEARTS_MAX_TOKENS_CAP=65536
CODEARTS_BENEFIT_FALLBACK=0
CODEARTS_MODEL_CACHE=C:\path\to\models-cache.json
```

| 变量 | 说明 |
|---|---|
| `CODEARTS_AK` / `CODEARTS_SK` | 华为云访问密钥，必填 |
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

## 客户端接入

```text
Base URL : http://127.0.0.1:8787/v1
API Key  : codearts      （任意非空字符串）
Model    : deepseek-v4.1-flash / glm-5.3 / kimi-k3 / openpangu-2.0-pro ...
```

## 子命令

```cmd
codearts2api.exe serve                 启动代理（默认）
codearts2api.exe models                列出当前账号可用模型及通道/上下文/输出上限
codearts2api.exe selftest [MODEL...]   对运行中的代理做端到端自检（非流式 + 流式）
codearts2api.exe benefit status        查询签到状态
codearts2api.exe benefit claim         签到领取当日免费额度
codearts2api.exe benefit balance       查询免费额度余额
codearts2api.exe benefit models        查询免费模型列表
```

`selftest` 可用 `CODEARTS_SELFTEST_BASE` 指向其他地址（默认 `http://127.0.0.1:8787/v1`）。

## 接口

| 端点 | 说明 |
|---|---|
| `POST /v1/chat/completions` | OpenAI 格式聊天补全，支持 `stream` 流式与 `tools` |
| `GET /v1/models` | 模型列表（含 context_window / max_tokens 等能力元数据） |
| `GET /v1/models/{id}` | 单模型详情 |
| `OPTIONS *` | CORS 预检 |

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

- **并发上限取决于账号套餐**，实测套餐为 3。超出时上游返回
  `TM.00001041 并发会话数已达上限`，代理会以 502 包装该信息。
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
