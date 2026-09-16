# Mausfer

Mausfer 是开源的跨平台文件传输工具，支持 **Windows、macOS 和 Android**。同一局域网自动发现设备；不同网络通过自建信令服务器配对，使用 WebRTC 传输文件。

最新发行版为 **v0.1.1**，包含 **0.1.0 客户端**和 **0.1.1 信令服务**。本次客户端代码与安装包保持不变；服务端兼容现有客户端和 `config.json`，优化了心跳、协商消息转发、慢连接处理和通信日志输出。

## 下载与安装

从 [Releases](https://github.com/TirededMaxer/Mausfer/releases/latest) 下载对应版本。发行包不放入源码仓库。

| 平台 / 组件 | 下载文件 | 使用方式 |
| --- | --- | --- |
| Windows x64 | `Mausfer-0.1.0-windows-x64.exe` | 便携版，直接运行；需要 WebView2 Runtime |
| macOS Apple Silicon | `Mausfer-0.1.0-macos-arm64.dmg` | 打开后将 Mausfer 拖入 Applications；另提供 App ZIP |
| Android 7.0+ | `Mausfer-0.1.0-android-universal.apk` | 安装 APK，包含 ARM64、ARMv7、x86_64 和 x86 |
| 信令服务器 | `Mausfer-0.1.1-signaling-server.zip` | 解压后运行 `java -jar mausfer-signaling.jar`，需要 Java 17+ |

Release 同时提供独立 JAR、示例 `config.json` 和 `SHA256SUMS.txt`。macOS 首版为本地签名，尚未经过 Apple 公证；如系统阻止打开，可在“系统设置 → 隐私与安全性”中核对来源并允许打开。Windows 首版未作商业代码签名。当前不提供 Intel Mac 或 Linux 桌面安装包。

## 主要功能

- 局域网自动发现设备，桌面端支持拖放选择文件。
- 远程连接码配对；界面配置自建信令服务器，并显示连接状态。
- 收发双方按接收端确认的字节同步进度。进度条位于文件选择区与日志之间，只在传输期间显示。
- SHA-256 文件校验、接收完成确认、中断续传及同名文件保护。
- 使用系统设备名称，每次进程启动生成随机设备 ID。
- 桌面端可修改下载目录；Android 接收文件保存到系统下载目录。
- Android 主界面返回键回到桌面；应用进程存活时继续当前任务，系统回收或强行停止仍会中断任务。
- 中文 / 英文界面。

## 快速开始

### 局域网

1. 在可以互相访问的同一网络中打开两端客户端。
2. 发送方选择文件，在“附近设备”中选择接收方并发送。
3. 等待完成日志，在接收端下载目录查看文件。

局域网不需要信令服务器。设备发现使用 UDP `43111`，文件传输使用 TCP `43110`；系统防火墙和 Wi-Fi 客户端隔离可能影响发现或连接。

### 不同网络

1. 自行部署 [Java 信令服务](signaling-server/README.md)，默认监听 TCP `38386`。
2. 两端填写同一完整地址，例如 `ws://signal.example.com:38386`；使用 TLS 反向代理时填写 `wss://signal.example.com`。
3. 接收方点击“开始接收”，把连接码发给发送方。
4. 发送方选择文件、填写连接码，点击“发送所选文件”。

客户端不预置开发者个人服务器，也不提供托管信令服务。默认公共 STUN 无需自行部署，但 STUN 不能保证穿透所有 NAT 或 UDP 限制；必要时在两端高级网络设置中配置自建 TURN。信令 JAR **不包含 TURN，也不转发文件内容**。

信令已连接并不表示文件数据通道已连接。如果配对成功但直连失败，请检查两端 VPN／代理、UDP 限制和 NAT 条件。公网部署优先使用 `wss://`，连接码只分享给目标设备。局域网 TCP 传输不提供应用层加密，应在可信网络使用；WebRTC DataChannel 使用加密传输。

## 文档与开发

- [客户端使用说明与故障排查](docs/usage.md)
- [信令服务部署、配置、通信日志与性能测试](signaling-server/README.md)
- [构建环境、开发与贡献指南](CONTRIBUTING.md)
- [版本记录](CHANGELOG.md)

| 目录 | 内容 |
| --- | --- |
| `core/` | Rust 核心：配置、发现、文件协议、信令和 WebRTC |
| `platforms/tauri-common/` | 三端共用 Tauri 命令与任务管理 |
| `platforms/windows/`、`platforms/macos/` | 桌面客户端 |
| `platforms/android/` | Android 客户端、文件选择与下载发布 |
| `ui/` | 共用界面 |
| `signaling-server/` | 独立 Java WebSocket 信令服务 |
| `docs/` | 用户文档 |

依赖版本由 `Cargo.lock` 和 Maven / Gradle 构建文件管理。构建缓存、签名文件、私有配置、日志和发行包均不提交；协议回归测试保留在源码中。

### 信令服务开发与验证

安装 JDK 17+、Maven 3.8+ 和 Python 3.11+，在仓库根目录执行：

```bash
mvn -f signaling-server/pom.xml verify
python3 -m unittest discover -s signaling-server/tests -v
```

构建产物为 `signaling-server/target/mausfer-signaling.jar`。回归测试覆盖双向协商、心跳、房间复用、异常消息、限流、慢接收端，以及日志输出堵塞时继续服务和退出；GitHub Actions 会执行这些检查。另提供[本机性能测试](signaling-server/README.md#回归与本机压测)，用于观察 CPU、内存和消息延迟，不代表生产环境容量保证。

## 许可证

[MIT License](LICENSE)。第三方依赖保留各自许可证。
