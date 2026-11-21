# zchat

**一个消息工作区，一套可调用接口。**

zchat 把现有 IM 的账号与会话带进同一个原生桌面。桌面使用
[zgpui](https://github.com/LIghtJUNction/zgpui)，平台接入使用
[zplugin](https://github.com/LIghtJUNction/zplugin) 插件，CLI 与 MCP 共用
同一执行入口。项目属于 [Project Z](https://github.com/LIghtJUNction/project-z)。

消息、会话、成员、附件、搜索、状态与通话拥有统一的操作名称。每个插件声明自己
实际实现的能力；平台专属能力通过 `native.invoke` 保留原始请求和响应。新增平台
可以发布独立插件，无需修改桌面主程序。

## 安装与启动

在 [Releases](https://github.com/LIghtJUNction/zchat/releases) 下载对应平台的包，或：

```sh
cargo install zchat --locked
zchat
```

只需要 CLI/MCP 的环境可以安装 `cargo install zchat --locked --no-default-features`。
源码构建使用 Rust 1.100.0。Linux 桌面需要 X11、OpenGL、FreeType、Fontconfig
和 C 编译器；`--features linux-software` 选择 zgpui 的 CPU 绘制后端。原生桌面
发行目标是 Linux、macOS、Windows 的 x86_64 与 ARM64。Android/iOS 的原生 UI
和商店发行尚未实现。

```sh
zchat doctor
zchat catalog
zchat schema
```

`doctor` 展示本机前置条件，`catalog` 展示后端、能力和限制，`schema` 展示统一操作。
账号配置成功与在线授权成功分别报告。zchat 初次启动为空工作区；不会虚构在线
账号或聊天记录。

## 平台接入

| 插件 | 实际接入方式 | 当前交付边界 |
| --- | --- | --- |
| Telegram | 官方 TDLib C JSON ABI | 账号授权、会话与消息、编辑/删除/反应、正常已读与输入状态、更新、原生 TDLib 调用。需要独立 `api_id` / `api_hash` 与 TDLib 动态库。 |
| Matrix | Client-Server HTTPS API | 会话、明文消息、编辑/删除/反应、已读、同步和原生 API。发送前检查房间加密状态；加密房间需要具备完整密钥管理的额外插件。 |
| XMPP | libstrophe C ABI | 按插件目录报告的能力连接公开 XMPP 网络。OMEMO、Jingle 媒体和扩展组合需要对应实现与服务器协商。 |
| SimpleX | 官方 CLI 的本地 WebSocket API | 保留官方核心的加密与账号状态，提供统一消息操作和完整原生命令入口。仅接受回环地址。 |
| Delta Chat | 官方 `deltachat-rpc-server` stdio JSON-RPC | 账号、会话、消息、事件与原生 RPC；配置、加密与邮件传输由官方核心维护。 |
| Threema Gateway | 独立 Gateway HTTPS API 身份 | 余额/身份、显式 Basic 模式发信与预先加密的 E2E 数据传输。个人账号、个人历史和接收回调未实现。 |
| Session | 独立插件扩展位置 | 完整 libsession 客户端适配尚未实现，目录明确返回 unavailable。 |

常用操作与完整原生接口是两层能力。`call.start` 等名称已经进入统一契约；插件
只会声明实际能执行的操作。桌面内嵌音视频媒体引擎、所有平台的完整 GUI 以及
所有平台的全部扩展仍需逐项实现和验证。未支持的请求返回结构化错误。

Telegram 内容的 AI 调用按照当前 [API Terms §1.5](https://core.telegram.org/api/terms)
关闭；人类桌面/CLI 保留客户端入口。频道赞助消息随历史一起返回，客户端按
官方协议展示与反馈。平台登录、服务条款和原始加密边界保持由对应平台负责。
官方接口来源见 [协议说明](docs/protocols.md)。

## 添加账号

桌面的 **Connections** 页面提供连接表单。CLI 使用本地 JSON 文件添加账号：

```json
{
  "homeserver": "https://matrix.example.org",
  "access_token": { "env": "MATRIX_ACCESS_TOKEN" }
}
```

```sh
zchat accounts add work matrix --settings matrix-account.json --ai-read
zchat invoke work account.status
zchat invoke work conversation.list --params '{"limit":30}'
```

文件参数 `--settings -` 从 stdin 读取。凭据可以使用 `{"env":"VARIABLE_NAME"}`
引用，避免把令牌写入命令行参数。Unix 配置文件以 `0600` 权限原子写入，默认位于
`$XDG_CONFIG_HOME/zchat/config.json` 或 `~/.config/zchat/config.json`；Windows 使用
`%LOCALAPPDATA%\zchat\config.json`。可通过 `ZCHAT_CONFIG` 或 `--config PATH` 覆盖。
桌面宿主不会把消息正文写入本地历史文件；官方后端可维护自己的数据库。

读取指定会话：

```sh
zchat invoke work message.list \
  --params '{"conversation_id":"!room:matrix.example.org","limit":30}'
```

所有输出都是 JSON。批量集成可使用 `zchat rpc`，每行输入一条统一请求，每行
输出带原始 `id` 的结果或错误。发信由用户明确点击或调用写操作触发。

## AI 与 MCP

```json
{
  "mcpServers": {
    "zchat": {
      "command": "zchat",
      "args": ["mcp"]
    }
  }
}
```

MCP 使用 Project Z 的 [zmcpio](https://github.com/LIghtJUNction/zmcpio)，提供可发现的
工具、JSON Schema、结构化结果和读写提示。默认工具集只读；账号仍需显式启用
`ai_read`。允许写入时，同时启用账号的 `ai_write` 与服务器的 `--allow-write`。
`native.invoke` 还需 `--allow-native`。MCP 的调用来源由宿主赋值，请求中的
`actor:"human"` 不会改变权限。

```sh
zchat accounts policy work --ai-read true --ai-write true
zchat mcp --allow-write
```

每次预期发送使用独立 `idempotency_key`。同一进程内对已成功的有界响应做重复请求
合并；Matrix 同时把该键传给服务器交易 ID。网络超时后的发送状态由平台决定，
通用接口不宣称跨所有后端提供恰好一次发送。外部消息文本、文件名和个人资料均为
不可信数据，不能当作调用 AI 的指令。详细示例见 [AI 接口](docs/ai.md)。

## 插件开发

`zchat-core` 是 `no_std + alloc` 协议核心，只依赖 Project Z 的 `zconv`。
`zchat-plugin-api` 冻结 `projectz.zchat.im` v1 接口：

```rust
#[zplugin::interface(id = "projectz.zchat.im", version = 1)]
pub trait ImPlugin {
    fn manifest(&self) -> zplugin::Result<zplugin::OwnedBytes>;
    fn invoke(&mut self, request: &[u8]) -> zplugin::Result<zplugin::OwnedBytes>;
}
```

原生插件返回能力目录，并接收有界 JSON 请求。对象与输出缓冲均由创建它们的
动态库释放，宿主通过 zplugin 的库引用保持生命周期。发布包同时提供内置后端
动态库，供集成与 ABI 验证使用；默认内置后端也可静态调用。

```sh
zchat plugins discover
zchat plugins inspect ./libzchat_providers.so --trust
zchat plugins add open-im ./libzchat_providers.so --trust
zchat plugin-header > zchat-im.h
```

`.dylib` 用于 macOS，`.dll` 用于 Windows。原生插件具有当前用户权限，`--trust`
是明确的本地注册动作；未知目录不会被自动扫描并执行。为插件账号添加
`--plugin open-im` 即可使用它。接口与边界见 [架构说明](docs/architecture.md)。

## 验证与发行

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
python3 scripts/acceptance.py --binary /path/to/zchat --plugin /path/to/libzchat_providers.so
```

验收用隔离配置和本地协议服务测试真实 CLI、MCP 与动态插件，不向真实联系人发送
测试消息。性能数据与平台结果必须以实际运行记录为准，不能从可编译推断已登录
真实平台。

GitHub Actions 的 **Release zchat** 支持 `current / patch / minor / major / custom`
一键发版。工作流使用 [zdistro](https://github.com/LIghtJUNction/zdistro) 生成平台
包和校验收据，并按依赖顺序发布四个 crate。平时推送不触发远端测试。
开发 manifest 始终使用 Project Z 最新默认分支的 Git 来源；发布暂存区把已经
锁定且与注册表源码一致的依赖投影成精确版本。生成包、截图和编译产物保存在
发行附件，不进入 Git 源码历史。详见 [发行操作](docs/releasing.md)。

## 许可证

zchat 使用 MIT。TDLib、SimpleX、Delta Chat、libstrophe 和其他独立后端保留各自
许可证与服务条款；zchat 的发行包不捆绑这些平台的官方客户端引擎。
