# 更新日志

## Unreleased

- 升级至 contract 2 / 1.0.0 并导入 transport 1.1.0；移除 manifest 网络宿主声明与 transport／MQTT／密码包装／Cookie jar 的 64KiB 限制。通用 HTTP 不设固定期限，登录 HTTP、WebSocket 连接和 MQTT 握手透传二维码协议剩余期限；HTTP 逐跳跟随重定向并按来源域、Domain、Path 与 Secure 保留中间 Cookie，播放交付最终 URL，MQTT 握手重定向保留有界跟随。MQTT 的 32 事件批次只用于短轮询让出调度，不再限制 SUBACK 前累计消息或帧内包数量。重新生成并锁定依赖。

- 项目代码使用 MIT 许可证，顶层许可证文件统一命名为 License，保留三方许可原文。

- 0.1.1 的 Release ZIP 根目录提供 manifest 和 Component；三方许可位于 licenses/ 下并统一命名为 License。
