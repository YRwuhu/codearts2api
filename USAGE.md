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
