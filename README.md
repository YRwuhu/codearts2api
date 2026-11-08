# CodeArts2API

把华为云 CodeArts 的模型接口转成 OpenAI 兼容 API 的本地代理。Rust 写的，编译完就一个 exe，不需要装别的运行时。

## 构建

```
cargo build --release
```

产物在 `target\release\codearts2api.exe`。也可以直接双击 `build.bat`。

## 快速开始

首次使用**不需要先准备账号**，直接起来再登录：

```
codearts2api.exe serve
```

或双击 `start.bat`（没有 `.env` 会自动建一个）。默认监听 `127.0.0.1:8787`。

启动后浏览器打开 http://127.0.0.1:8787/ ，进「账号」页点「登录华为云」，扫码或手机号登录一次即可。登录凭证存 `auths/` 并自动续期。

## 配置

账号有两种来源，任选其一；也可以都不配，走上面的 WebUI 登录。

**OAuth 登录（推荐）**：在面板「账号」页点「登录华为云」，不用手填密钥。

**手动 AK/SK**：根目录建个 `.env`：

```ini
CODEARTS_AK=你的AccessKey
CODEARTS_SK=你的SecretKey
```

> 记得把占位符换成真实密钥。华为云 AK/SK 全是字母数字，
> 示例里的中文占位符会被识别出来并跳过，不会拿去签名。

多账号的话复制 `accounts.example.json` 为 `accounts.json`，往里加就行。这个文件不会被 git 提交。

## 运行

```
codearts2api.exe serve
```

或双击 `start.bat`。默认监听 `127.0.0.1:8787`。

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
- 模型目录优先读取缓存，过期后后台单飞刷新，不阻塞聊天首字。
- 同一对话会固定路由到同一账号（prompt cache 是账号级的）。
- 支持模型原生识图：图片直接内嵌发送，不再绕网页附件会话；纯文本模型收到图片会自动回退视觉模型（`CODEARTS_VISION_MODEL`，设为 `off` 可关闭）。
- 上游内容审核误拦截（`InferHub.ModelArts.81011.403`，返回一段固定拒答）会自动立即重试，最多 5 次，不计账号失败。
- `/v1/responses` 完整兼容 Codex / ZCode：思维链走 reasoning summary 事件，`response.completed` 带齐 usage，不会把思维链当正文显示。
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
| `CODEARTS_SANITIZE` | 开 | 设为 `0` / `off` 关闭提示词清洗 |
| `CODEARTS_SANITIZE_RULES` | `sanitize.json` | 自定义规则文件路径（不存在则用内置规则） |

其余的看 `.env.example`。

### 提示词清洗（WAF 误拦截）

ZCode 会把仓库的 git 上下文注入到消息里，其中这一句会被上游内容安全网关判成
提示词注入，整单返回 `InferHub.ModelArts.81011.403` 和一段固定拒答：

```
Main branch (you will usually use this for PRs): master
```

代理在发往上游前把它改写成等价的 `Default git branch: master`（移植自
[honlnk/zcode-prompt-sanitizer](https://github.com/honlnk/zcode-prompt-sanitizer)，MIT）。
只改请求、不碰响应，默认作用于 `system` 和 `user` 消息；命中时打印
`[sanitize] 已改写 …`。

需要更多规则时在代理目录放一个 `sanitize.json`（**一旦提供就整体替换内置规则**）：

```json
{
  "rules": [
    {
      "id": "my-phrase",
      "enabled": true,
      "scopes": ["system", "user"],
      "match": "要替换的原文",
      "replacement": "替换后的文本"
    }
  ]
}
```

`scopes` 可选 `system` / `user` / `assistant` / `tool`；`replacement` 留空表示删除。
匹配是纯字面量，不支持正则。

## 测试

```
cargo test           单元测试
cargo run -- models  用真实凭证验证签名和模型同步
```

## 免责声明

独立技术研究项目，与华为云无关。用出问题（封号、额度消耗等）自己负责。
