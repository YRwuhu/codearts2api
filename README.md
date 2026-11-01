# CodeArts2API

把华为云 CodeArts 的模型接口转成 OpenAI 兼容 API 的本地代理。Rust 写的，编译完就一个 exe，不需要装别的运行时。

## 构建

```
cargo build --release
```

产物在 `target\release\codearts2api.exe`。也可以直接双击 `build.bat`。

## 配置

两种方式，任选其一。

**OAuth 登录（推荐）**：启动后在面板「账号」页点「登录华为云」，浏览器扫码或手机号登录一次就行，不用手填密钥。凭证存 `auths/`，自动续期。

**手动 AK/SK**：根目录建个 `.env`：

```ini
CODEARTS_AK=你的AccessKey
CODEARTS_SK=你的SecretKey
```

多账号的话复制 `accounts.example.json` 为 `accounts.json`，往里加就行。这个文件不会被 git 提交。

## 运行

```
codearts2api.exe serve
```

或双击 `start.bat`。默认监听 `127.0.0.1:8787`。

启动后浏览器打开 http://127.0.0.1:8787/ 有个管理面板，能看账号状态、模型列表、额度用量，也能直接测模型。

## 客户端接入

```
Base URL: http://127.0.0.1:8787/v1
API Key:  codearts    （随便填个非空的就行）
```

支持三种协议，都指向同一个 Base URL：

- OpenAI Chat Completions：`POST /v1/chat/completions`
- OpenAI Responses（Codex）：`POST /v1/responses`
- Anthropic Messages（Claude Code）：`POST /v1/messages`

## 常用命令

```
codearts2api.exe serve           启动代理 + 面板
codearts2api.exe accounts        看账号和模型归属
codearts2api.exe models          看可用模型
codearts2api.exe selftest        对运行中的实例做端到端自检
codearts2api.exe benefit status  看签到状态
codearts2api.exe benefit claim   手动签到领额度
```

## 一些值得知道的事

- 多账号时模型列表自动合并去重，请求按顺序故障切换。
- 同一对话会固定路由到同一账号（prompt cache 是账号级的）。
- 服务启动后自动签到领每日免费额度，不用管。
- 默认只监听本机。改成 `0.0.0.0` 的话同网段谁都能用你的额度，自己掂量。
- 用量数据存在 `usage.jsonl`，面板里能看趋势和明细。

## 环境变量

常用的几个：

| 变量 | 默认 | 说明 |
|---|---|---|
| `CODEARTS_PROXY_PORT` | `8787` | 监听端口 |
| `CODEARTS_PROXY_HOST` | `127.0.0.1` | 监听地址 |
| `CODEARTS_CONCURRENCY` | `8` | 本地并发上限 |
| `CODEARTS_DAILY_TOKEN_CAP` | `0` | 每账号每日 token 上限，0 不限 |

其余的看 `.env.example`。

## 测试

```
cargo test           单元测试
cargo run -- models  用真实凭证验证签名和模型同步
```

## 免责声明

独立技术研究项目，与华为云无关。用出问题（封号、额度消耗等）自己负责。
