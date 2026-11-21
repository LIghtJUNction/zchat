# 发布 zchat

zchat 的开发源码直接使用 Project Z 默认分支。crates.io 发布从已提交的
`Cargo.lock` 生成独立目录，把内部 Git 依赖投影为对应的精确注册表版本；
开发目录中的依赖声明保持原样。

## 一键发版

在 GitHub Actions 运行 **Release zchat**，选择 `patch`、`minor`、`major`
或 `custom`。`custom` 需要填写比当前版本更新的 `MAJOR.MINOR.PATCH`。
首次发布和恢复失败的发行使用 `current`。工作流需要仓库的
`CARGO_REGISTRY_TOKEN` secret，以及默认分支的提交、标签和 Release 写入权限。

工作流按以下顺序执行：

1. 同步四个 crate、工作区内部依赖和 `zdistro.toml` 的版本；生成锁文件，运行
   Rust 格式检查、无界面测试和发布脚本测试。
2. 提交版本变更，把默认分支与版本标签原子推送。默认分支在此期间推进时，
   操作失败并保留候选提交，不覆盖远端变更。
3. 在 Linux、macOS、Windows 的 x86_64 和 ARM64 原生 runner 上测试、构建应用
   与插件，用真实产物完成本机 Matrix 端到端验收，再使用当前 zdistro 默认分支
   打包和验证。
4. 校验 Project Z 注册表依赖，按 `zchat-core`、`zchat-plugin-api`、
   `zchat-providers`、`zchat` 的顺序发布 crates.io。
5. 核对六个平台的源提交、包摘要、zdistro 收据和注册表收据，创建或恢复
   GitHub Release 草稿，上传包、crate 归档与验证材料。复核服务器记录的资产
   大小与摘要后，把草稿发布为正式 Release。

推送 `vMAJOR.MINOR.PATCH` 标签也会执行正式发行。普通分支 push 与 pull request
不会触发发行或例行远端 CI。GitHub 自动生成的提交使用本次受检源码；发布过程中
不会执行 `git push --force`。

## 依赖身份校验

`scripts/release.py stage` 从提交归档生成 staging 目录，并检查每个内部 Git
依赖的 Cargo package 名称、版本、仓库、完整 Git 提交和干净的源码检出。
对应注册表归档必须匹配 crates.io 给出的 SHA-256。

比对范围由 `cargo package --list` 计算，包含 Rust/C/Objective-C 源文件、
`build.rs`、资源和编译辅助文件。新增或变更的编译文件、依赖和 feature 会阻止
发布。上游 README、CHANGELOG、`docs/` 文案和 package 描述、关键词等展示
元数据可以独立修改；这些豁免会记录在收据中。许可证、源码范围中的非文档
文件和依赖设置没有此豁免。

投影后的解析必须接受原锁文件指定的版本图，且不得残留 Git 依赖。若最新
Project Z 源码尚无匹配的 registry 包，先发布该上游版本，再执行 zchat 发布。
脚本不会选择旧 Git 提交或换用同名旧版注册表包。

每个 zchat crate 内包含 `release-source.json`，记录源提交和开发锁文件摘要。
重复执行发布时，已存在的同版本 crate 必须匹配完整 zchat 发布源码、manifest
及该来源记录。版本存在本身不会使脚本跳过校验。staging 后修改源码也会使发布
失败；需要重新从提交生成目录。

## 本地发行命令

Python 3.12+、Git、Rust 1.100.0 和平台编译工具需要已安装。日常开发构建可继续
使用用户配置的共享 Cargo target；正式多平台工作流使用各自 runner 的临时目录。

```sh
python scripts/release.py check-version
python -m unittest discover -s scripts/tests -v
cargo test --workspace --locked --no-default-features
cargo build --workspace --release --locked
```

更新版本后，提交源码和锁文件，再创建注册表 staging：

```sh
python scripts/release.py bump --bump patch
git add Cargo.toml Cargo.lock crates zdistro.toml
git commit -m 'release: prepare next zchat version'
python scripts/release.py stage --output /tmp/zchat-registry-stage
python scripts/release.py publish --stage /tmp/zchat-registry-stage
```

staging 路径必须是源码目录之外的新目录。发布命令通过正常 Cargo 凭据机制或
`CARGO_REGISTRY_TOKEN` 环境变量认证。`--dry-run` 使用 Cargo 的实际打包验证，
尚未存在于 registry 的工作区依赖仍会阻止下游的独立 package 验证。

成功后，staging 中的 `publish-receipt.json` 记录四个 crate 的注册表复核结果；
`published-crates/` 保存实际下载并核对的 registry 归档，恢复发行时也会重建此
目录。发布脚本不把这些产物写入 Git。

## 本机端到端验收

`scripts/acceptance.py` 使用传入的真实可执行文件和动态插件，在 `127.0.0.1`
临时启动一个 Matrix HTTP 模拟服务器。它没有真实 IM 账号，不使用第三方服务
凭据；两次成功发送分别来自本地 CLI 和获授权的 MCP，接收方都是模拟服务器。

```sh
python scripts/acceptance.py \
  --binary target/release/zchat \
  --plugin target/release/libzchat_providers.so \
  --receipt /tmp/zchat-acceptance.json
```

使用共享 Cargo target 时替换以上两个路径。验收覆盖账号配置与凭据隐藏、中文
和 emoji 消息、会话同步、真实 zplugin 加载与账号路由、MCP 握手和工具发现、
默认写入禁用、账号权限、伪造 actor、重复 key 不发送、改变 payload 返回冲突，
以及普通和 native 调用的加密房间明文保护。临时配置在 POSIX 平台要求 `0600`，
进程结束后删除；Windows 记录该权限检查不适用。

stdout 和可选文件输出 `zchat.acceptance.v1` JSON 收据，包含产物 SHA-256、
通过用例和 HTTP/发送计数；失败会返回非零状态。幂等验收覆盖同一 MCP 会话；
宿主缓存仅保留最近 32 个、请求与结果合计不超过 32 KiB 的写操作。跨重启的
Matrix 去重依赖 homeserver 对 transaction ID 的实现。

## 使用 zdistro 打包

`scripts/package.py` 接收已构建的应用和插件，检查 ELF、PE 或 Mach-O 的真实
机器架构、应用版本与宿主系统，再生成平台文件布局。zdistro 负责原生格式生成
与验证，脚本随后对移动到交付目录的产物重新执行 `zdistro verify`。

```sh
git clone --depth 1 https://github.com/LIghtJUNction/zdistro.git /tmp/zchat-zdistro
zchat_target="$(cargo metadata --no-deps --format-version 1 | python -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
python scripts/package.py \
  --binary "$zchat_target/release/zchat" \
  --plugin "$zchat_target/release/libzchat_providers.so" \
  --zdistro-root /tmp/zchat-zdistro \
  --out dist/release
```

Windows 使用 `zchat.exe` 与 `zchat_providers.dll`，macOS 使用 `zchat` 与
`libzchat_providers.dylib`。已有 zdistro 检出需先更新到当前默认分支；打包收据
记录实际使用的 commit。`--formats` 可指定以逗号分隔的格式，缺少原生工具会
明确失败，不会把跳过的目标记录为成功。

| 原生平台 | 工作流格式 | 运行与安装布局 |
| --- | --- | --- |
| Linux x86_64 / ARM64 | `.tar.gz`、`.zip`、`.deb` | 便携归档；Deb 安装 `/usr/bin/zchat` 与 `/usr/lib/zchat/` 插件 |
| macOS Intel / Apple Silicon | `.zip`、`.tar.gz`、`.pkg`、`.dmg` | `zchat.app`；PKG 安装到 `/Applications`，并提供 `/usr/local/bin/zchat` |
| Windows x86_64 / ARM64 | `.zip`、`.msi` | 可执行文件与插件 DLL 同目录；MSI 使用原生 Windows Installer |
| Arch Linux | 本地 `--formats arch` | 需要 `makepkg`、`fakeroot`、`pacman` 与 `bsdtar`；产物为 `.pkg.tar.gz` |

每个包都有独立 `.receipt.json`，同平台同时提供原始 `.zdistro.toml` 和
`.provenance.json`，工作流另附 `.acceptance.json`。Release 汇总会比对验收与
打包的应用及插件摘要，避免以一个产物的验收结果替另一个产物背书。文件名包含
平台与架构，可直接用于 Release 资产。验证命令：

```sh
python -m zdistro verify zchat-VERSION-linux-x86_64.deb.receipt.json \
  --manifest zchat-VERSION-linux-x86_64.zdistro.toml
```

包验证覆盖格式、文件内容和身份，安装收据的 `installed` 字段保持 `false`。
当前桌面产物未签名或 notarize。Android、iOS 和浏览器需要各自的应用宿主与
协议后台集成，当前工作流不为这些目标生成空应用包。

## 恢复失败的发布

保留失败输出并修复具体原因。相同源码可重新运行对应 tag 的工作流；也可在
默认分支没有后续提交时选择 `current`。若代码必须修改，使用新的版本，避免把
已发布的不可变 crate 版本对应到不同源码。

GitHub Release 汇总步骤要求六个原生目标和四个 registry 包全部有验证材料。
某个平台失败时，其余已生成产物保留在 Actions artifacts，最终 Release 汇总
步骤不会执行。所有外部引擎、账号认证和平台能力边界见项目 README。

## 官方工具参考

- [Cargo package](https://doc.rust-lang.org/cargo/commands/cargo-package.html)
- [Cargo publish](https://doc.rust-lang.org/cargo/commands/cargo-publish.html)
- [zdistro 格式与验证](https://github.com/LIghtJUNction/zdistro/blob/main/docs/formats.md)
- [GitHub 原生 runner 与架构](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [actions/checkout](https://github.com/actions/checkout)、[setup-python](https://github.com/actions/setup-python)
- [upload-artifact](https://github.com/actions/upload-artifact)、[download-artifact](https://github.com/actions/download-artifact)
