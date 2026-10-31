# 快速开始

三步：下载 → 登录 → 接入。

## 1. 下载

从 [Releases](https://github.com/YRwuhu/codearts2api/releases/latest) 下载
`codearts2api.exe`，放到任意目录，例如 `D:\tools\codearts2api\`。

单个 exe，约 8 MB，**不需要安装 Python 或任何运行时**。

## 2. 启动

双击 `codearts2api.exe`，或在该目录打开命令行：

```cmd
codearts2api.exe serve
```

首次启动会生成 `.env` 和 `accounts.json`（如果不存在）。看到下面这行就说明起来了：

```
CodeArts OpenAI 兼容代理已启动: http://127.0.0.1:8787
管理界面: http://127.0.0.1:8787/
```

## 3. 登录华为云（推荐）

浏览器打开 **http://127.0.0.1:8787/**，左侧点「账号」，再点 **登录华为云**。

会弹出华为云授权页，用手机号或扫码登录一次即可。凭证自动保存到 `auths/`，
有效期 24 小时，到期前自动续期。**不需要填任何 AK/SK。**

> 如果浏览器没有自动弹出，点「打开授权页」手动打开。
> 登录完成后回到面板，账号会自动出现在列表里。

### 备选：手动 AK/SK

不想走登录，也可以在「账号」页点「添加账号」，填入华为云 AK/SK。
密钥获取：https://console.huaweicloud.com/iam/#/mine/accessKey

## 4. 接入客户端

把任何支持 OpenAI 兼容接口的工具指向：

```
Base URL : http://127.0.0.1:8787/v1
API Key  : codearts        （任意非空字符串）
Model    : deepseek-v4.1-flash
```

三种协议都支持，Base URL 相同：

| 客户端 | 端点 | 备注 |
|---|---|---|
| OpenAI 兼容（Cline / Cherry Studio / Roo ...） | `/v1/chat/completions` | 最通用 |
| Codex / Responses SDK | `/v1/responses` | `max_output_tokens` 上限 131072 |
| Claude Code / Anthropic SDK | `/v1/messages` | 设 `ANTHROPIC_BASE_URL=http://127.0.0.1:8787` |

推荐模型：

| 模型 | 说明 |
|---|---|
| `deepseek-v4.1-flash` | 综合最好，速度快 |
| `glm-5.3` | 推理强，但思维链占输出额度，`max_tokens` 建议 ≥ 2000 |
| `kimi-k3` | 响应稳定 |
| `openpangu-2.0-pro` | 长上下文（512K） |

具体可用模型以面板「模型」页为准（跟着账号套餐走）。

### 常见客户端配置

**Cherry Studio / LobeChat / Cline / Continue**：选「自定义 OpenAI」，
填上面的 Base URL 和 API Key。

**OpenCode / OpenClaw**：在配置文件里加一个 openai-compatible provider。

## 面板功能

| 页面 | 用途 |
|---|---|
| **概览** | 账号、模型数、并发与会话状态 |
| **账号** | 登录、增删账号、启停、健康状态 |
| **模型** | 模型列表 + 请求测试 |
| **额度** | 套餐 credit 与 token 消耗，附华为云控制台链接 |
| **用量** | 请求数、token、耗时、趋势图、最近明细 |
| **签到** | 每日签到（自动执行，也可手动触发） |

## 自动做的事

启动后无需干预：

- 每 30 分钟检查一次签到，当天未签自动领取
- 每 10 分钟刷新一次额度缓存
- OAuth 凭证到期前 30 分钟自动续期
- 每次请求的用量记录到 `usage.jsonl`

## 并发说明

上游按**会话**计并发（当前套餐上限 3）。代理已处理：

- 每账号并发上限 3（`CODEARTS_CONCURRENCY` 可调），超出排队而非报错
- 同一对话固定路由到同一账号（会话钉号），避免重复占槽位
- 撞到上限的账号冷却 20 秒，自动切到其他账号

实测同时 10 个请求全部成功。

## 配置项

在 exe 同目录创建 `.env` 可覆盖默认值：

```ini
# 监听地址与端口
CODEARTS_PROXY_HOST=127.0.0.1
CODEARTS_PROXY_PORT=8787

# 每账号并发上限（默认 3）
CODEARTS_CONCURRENCY=3

# max_tokens 兜底上限（模型元数据缺失时用）
CODEARTS_MAX_TOKENS_CAP=65536

# 免费额度模型拿不到时是否仍列出兜底清单
CODEARTS_BENEFIT_FALLBACK=0
```

## 子命令

```cmd
codearts2api.exe serve             启动代理 + 面板（默认）
codearts2api.exe accounts          列出账号与模型归属
codearts2api.exe models            列出可用模型
codearts2api.exe selftest          对运行中的代理做端到端自检
codearts2api.exe benefit claim     签到
```

## 文件说明

运行后会在同目录产生：

| 文件 | 内容 | 是否含密钥 |
|---|---|---|
| `.env` | 配置 | 手动 AK/SK 时含 |
| `accounts.json` | 账号列表 | OAuth 账号不含 |
| `auths/` | OAuth 凭证（0600） | **是，勿外传** |
| `usage.jsonl` | 请求用量记录 | 否 |
| `models-cache.*.json` | 模型列表缓存 | 否 |

## 排错

**端口被占用**
```cmd
netstat -ano | findstr :8787
taskkill /F /PID <上面查到的PID>
```

**账号显示"凭证失效"**
OAuth 会话过期，到「账号」页重新点「登录华为云」。

**请求返回 401 APIG.0301**
手动 AK/SK 填错，或 OAuth 凭证已失效。看面板「账号」页的健康列。

**上游报并发上限**
已自动排队/切换，若持续出现可调低 `CODEARTS_CONCURRENCY`。

## 说明

本项目仅用于技术研究与学习。每次调用会消耗华为云 CodeArts 套餐额度。
与华为云官方无关联，使用风险自负。

## 常见问题

### 输出只有 6 万多 token，官方说能到 38 万？

上游单次输出硬上限是 **131072**，不是 384k（384k 是模型总上下文量级，
或者厂商宣传口径）。超过 131072 上游会直接报
`max_tokens should be between 1 and 131072`。

本代理按 131072 放行，并在 `/v1/models` 里如实报告。
早期版本误信模型元数据里的 65536 而把请求砍半，现已修正。

如果客户端仍然只拿到 6 万，检查是不是客户端自己设了较小的
`max_tokens`（例如 8192 或 16384）。

### 报"并发会话数已达上限"（TM.00001041）

**请先升级到 v0.4.2 或更高。** 这个问题已在 v0.4.2 修复。

原因：上游用 `x-ot-session-id` / `user-session-id` 识别会话，每账号最多 3 个。
旧版不带这两个头，上游就把每个请求都当成新会话，连续 3 次后第 4 次必被拒——
即使是串行的。

修复：代理现在给同一账号的所有请求带同一个固定会话 ID，该账号永远只占
1 个上游会话。实测 8 个并发流式请求全部成功，5 个会话各 3 轮共 15 个请求
全部成功。

如果升级后仍然遇到，说明有别处（另一个客户端、另一个代理实例、或直接调用）
也在用同一个华为云账号，一起把会话数推过了 3。

### 工具调用 / 编辑文件行为异常

如果用的是 `v0.4.0` 之前的版本，请升级。旧版 Anthropic 流式适配有两个
协议错误，会直接导致工具调用解析失败和内容重复：

- content block 的 `index` 从 1 开始（Anthropic 要求从 0 开始）
- thinking block 从不发送 `content_block_stop`，客户端会一直卡在那里

另外旧版在"名额已满"时会静默降级为非流式，客户端看到的是本地合成的
伪流式，工具调用的增量帧全部丢失。现在默认不降级。

### Claude Code 连不上 / 报 404

确认 `ANTHROPIC_BASE_URL` 是 **不带** `/v1` 的
`http://127.0.0.1:8787`，Claude Code 会自己拼 `/v1/messages`。
