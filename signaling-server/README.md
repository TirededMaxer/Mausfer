# Mausfer 信令服务

轻量 Java WebSocket 服务，用于 Mausfer 客户端交换连接信息。JAR 已包含依赖，不需要 Maven、数据库或额外守护进程。运行环境为 Java 17 或更新版本。

## 启动

将 `mausfer-signaling.jar` 和 `config.json` 放到服务器同一目录：

```json
{
  "port": 38386
}
```

```bash
java -jar mausfer-signaling.jar
```

首次启动如果没有配置文件，会在 **JAR 所在目录**自动生成上述配置；从其他工作目录启动也相同。修改端口后重启生效。端口必须是 1–65535 的整数，占用或配置错误会报告原因并以非零状态退出。按 Ctrl+C 关闭连接并释放端口。

这是一项前台服务，首次启动完成配置初始化，不会修改系统服务、自动安装软件或防火墙规则。若提示找不到 Java，请先通过服务器操作系统的软件包管理器安装 Java 17 或更新版本。

## 客户端连接

1. 在云安全组和服务器防火墙中允许服务的 TCP 端口，默认 `38386`。
2. 双方在 Mausfer 的“信令服务器”中填写同一个 `ws://你的服务器地址:38386` 并保存。
3. 接收方开始接收，把连接码发送给发送方；发送方选择文件并输入连接码。

客户端不会内置开发者的私有服务器地址，也不会回退到公共 MQTT 服务。每位部署者使用自己的服务器。

公网长期部署建议使用域名和 TLS 反向代理，客户端填写 `wss://你的域名`。例如已安装 Caddy 时，可使用以下 Caddyfile：

```caddyfile
signal.example.com {
    reverse_proxy 127.0.0.1:38386
}
```

将示例域名替换为解析到服务器的域名，为 Caddy 开放 TCP 80/443。此时将后端 38386 端口限制为服务器本机访问，并在客户端填写 `wss://signal.example.com`。Caddy 是可选的独立软件，不包含在 JAR 中。

## 信令与文件传输

服务只交换设备信息、SDP 和 ICE 元数据，不存储或转发文件。连接码是临时房间凭据，应只发给接收对象。每个房间最多两端；第三端、重复身份和无效消息会被拒绝。连接断开后移除房间成员，无成员房间自动释放。未加入房间的连接最多保留 15 秒，信令会话最多保留 20 分钟。

WebRTC 会尝试直连。运营商 NAT、UDP 封锁等条件可能使直连失败；**更换信令服务器不能替代 TURN 中继**。需要时自行部署 TURN 服务（例如 coturn），然后在两端“高级网络设置”中填写 TURN 地址、用户名和密码。JAR 不包含 TURN 服务。STUN/TURN 地址可以用逗号分隔多个地址。

## 从源码构建

安装 JDK 17+ 和 Maven 3.8+，在本目录执行：

```bash
mvn clean package
java -jar target/mausfer-signaling.jar
```

首次构建需要下载 Maven 依赖；部署只需复制 `target/mausfer-signaling.jar` 和 `config.json`。可直接在 IntelliJ IDEA 中打开本目录的 `pom.xml`，选择作为项目打开并加载 Maven 项目。主类为 `io.mausfer.signaling.Main`。

依赖：Java-WebSocket（MIT）、Gson（Apache-2.0）、SLF4J（MIT）。构建文件固定依赖版本。帧大小、连接数量、消息速率及待发送队列均设有上限；面向公网仍应按自己的负载需求配置反向代理连接限制。

## 连接状态与日志

客户端保存地址后会自动连接并持续发送心跳，显示“连接中”“已连接”或失败原因；失败后自动重试。连接状态只表示信令服务可用，文件数据通道的建立情况显示在客户端传输日志中。

升级时同时更新客户端和 JAR；旧 JAR 不支持新的心跳消息。按 Ctrl+C 停止旧进程，替换 JAR 后重新运行原启动命令即可，保留原 config.json。

服务端终端会记录 WebSocket 连接、加入房间、配对、offer/answer 转发、断开和拒绝原因。不会输出连接码、SDP、ICE 地址或文件内容。若客户端报域名解析或握手失败且服务端没有连接日志，应检查域名解析、云端口映射及反向代理的 WebSocket 配置。客户端日志中的 STUN 候选数量为 0 表示该次未获得 STUN 公网地址；STUN 可用仍不保证所有 NAT 网络都能直连。

客户端默认使用 Cloudflare 和 Google 公共 STUN。Cloudflare 官方公布的 STUN 地址为 `stun:stun.cloudflare.com:3478`，详见[官方说明](https://developers.cloudflare.com/realtime/turn/)。STUN 不转发文件，也不等同于 TURN。
