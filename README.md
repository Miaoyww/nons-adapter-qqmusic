# QQ 音乐读取适配器

来源 `qqmusic`，协议以 Rust 实现并编译为 WASM Component。应用通过统一 AdapterManager 加载；网络、时钟、安全随机数、WebSocket、设备状态和账号凭据经既有通用 Host 访问。运行不需要 Node、外部服务或本地端口。

当前源码包含搜索、曲目／专辑／艺人／歌单读取、个人曲库、基于“我喜欢”歌单的只读收藏分页、QQ 音乐 App／微信扫码、凭据刷新，以及标准／HQ／FLAC 明文播放资源解析。搜索遇到轻量接口错误码 `2001` 时，仅回退一次同平台桌面搜索。尚未声明收藏写入、歌单写入、推荐、下载、歌词或加密音频转换能力。

Rust 组件构建、模型／密码／协议回归、受控 Host 测试和 Windows 宿主回归已通过，公网搜索与歌曲详情已验证。QQ／微信已通过真实二维码初始化及取消清理，QQ 同时验证 MQTT 连接与订阅；真实手机确认、登录后的公网曲库和实际音频播放仍需验证。完整结果及复验方法见 [验收记录](ACCEPTANCE.md)。

## 文件与边界

| 文件                             | 职责                                             |
| -------------------------------- | ------------------------------------------------ |
| `src/lib.rs`、`src/component.rs` | 统一 JSON transport、Host 绑定、组件生命周期     |
| `src/catalog.rs`、`src/model.rs` | 目录业务、公开 DTO、分页和明文资源校验           |
| `src/auth.rs`、`src/auth/`       | 账号、Cookie、刷新、扫码与 MQTT 协议             |
| `src/device.rs`                  | Android 设备身份、QIMEI 与会话引导               |
| `src/crypto.rs`                  | 有界 RustCrypto 包装，不向宿主增加密码或平台方法 |

账号与不透明凭据统一交给 AccountManager；设备状态写入适配器专属 Host 分区，保存时剔除 `session*` 字段。曲目 MID、数字歌曲 ID 和音频 media ID 分开处理；缺少 MID 的目录曲目按数字 ID 补全，明确不存在的曲目保留不可用占位。游标绑定请求和页大小。

组件导入 `nons:host/transport@1.1.0`。Manifest 不再声明网络主机，HTTP／WebSocket 目标不受主机、IP、端口、URL 凭证或私网限制。通用 HTTP 以 `timeout-ms = 0` 不设期限，登录流程向 HTTP、WebSocket 连接和 MQTT 握手透传二维码协议剩余期限。HTTP 由适配器逐跳跟随，以 URL 集合检测循环并保留每跳 `Set-Cookie`；Cookie jar 不设总量上限，按来源域、Domain、Path 与 Secure 约束回放，播放 HEAD 的最终 URL 才进入交付结果。Host WebSocket 不支持重定向，MQTT 握手的服务器引用仍在同一 endpoint 下最多跟随三次。消息／帧不设固定字节上限，由 Host 队列实施背压。

JSON transport 不设固定 64KiB 上限；RustCrypto 包装也不再限制固定消息大小，只保留算法自身边界。播放仅接受已识别的明文音质前缀与扩展名，并以曲目目录中的完整大小核对最终资源 HEAD。拒绝试听、未知完整性、加密文件和禁止的音质降级。平台分页、签名、音质与 HKDF／RSA 参数限制继续保留；MQTT 每次短轮询最多处理 32 个事件后让出调度，SUBACK 前的累计消息和同一 WebSocket 帧内的包数量不受 32 限制。播放地址与凭据不进入公开元数据或诊断正文；缓存与失效沿用宿主规则。

## 构建

准备项目要求的 Rust 工具链、`wasm32-unknown-unknown` target 和 cargo-component 0.21.1。在仓库根目录执行：

```powershell
node scripts/build.mjs
```

构建使用本目录 Cargo.lock，生成 `dist/adapter.component.wasm`，并将 Component、描述符与许可复制到应用随包资源。正常 Tauri 开发／打包入口调用同一脚本；QQ 构建不再需要 npm 协议包、ComponentizeJS、WAC 或组合密码组件。其他 JS 适配器仍按各自构建要求安装依赖。

## 测试入口

```powershell
npm ci --ignore-scripts
cargo test --manifest-path Cargo.toml --locked
node scripts/build.mjs
node --test tests/component.test.mjs
```

适配器自身没有 npm 依赖，`package-lock.json` 仍保持可由 `npm ci` 复现；Rust 依赖由 `Cargo.lock` 和 `--locked` 固定。推送到 `main` 或提交 Pull Request 会运行 GitHub Actions；推送 `v*` 标签会在 Rust 测试、Component 构建和 Component harness 通过后发布可直接安装的 ZIP 及 SHA-256 校验文件。

Component 测试必须先重新构建 Rust 产物；jco 与 WASIShim 沿用 `packages/wasm-crypto` 的锁定开发工具依赖。Rust 模型单测和受控 Host 测试不能替代真实手机扫码、刷新、音质与播放验收。

## 协议依据与许可

平台协议依据 `@yakult-green-tea/qq-music-api@3.1.4`，保留 [MIT 许可](licenses/qq-music-api/License)。密码算法使用 RustCrypto，MQTT 数据包使用 mqttbytes 编解码，日期使用 time；依赖版本由 Cargo.lock 固定，构建脚本收集 WASM 生产依赖的许可证为 `licenses/dependencies/License`。

## 项目许可证

本项目代码采用 [MIT](License)。qq-music-api 的 MIT 条款保留在 `licenses/qq-music-api/License`，构建依赖告知位于产物的 `licenses/dependencies/License`。
