# 构建与开发

## 环境

- Rust 1.98.0 和 Cargo（当前发行构建与 CI 验证版本）；仓库包含 `Cargo.lock`，使用 `--locked` 保持依赖一致。CI 固定工具链，避免自动升级编译器后新增诊断改变已有版本的发布结果。
- 桌面客户端使用 Tauri 2。macOS 需要 Xcode Command Line Tools；Windows 需要 Visual Studio C++ Build Tools、Windows SDK 和 WebView2 Runtime。
- Java 服务需要 JDK 17+、Maven 3.8+。
- Android 构建需要 JDK 21、Python 3、Android SDK 36、NDK r26d，以及 Gradle Wrapper。

## 核心与桌面

在仓库根目录运行：

```bash
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

网络回归测试会绑定本机 TCP/UDP 端口，需要允许本机套接字操作。

安装 Tauri CLI：

```bash
cargo install tauri-cli --version '^2' --locked
```

在对应平台上构建：

```bash
cd platforms/macos    # Windows 则进入 platforms/windows
cargo tauri build
```

生成文件位于仓库 `target/release/bundle/`。签名、公证和 Windows 安装包签名由发布者使用自己的凭据配置。

命令行工具也可独立构建：

```bash
cargo build -p mausfer-core --bin mausfer --release --locked
```

## Android

以下脚本支持 macOS/Linux 构建主机。设置自己机器上的 SDK、NDK 和 JDK 路径，不要把绝对路径提交到仓库：

```bash
export JAVA_HOME=/path/to/jdk-21
export ANDROID_HOME=/path/to/android-sdk
export ANDROID_NDK_HOME=/path/to/android-ndk-r26d
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
python3 platforms/android/prepare_gradle.py
bash platforms/android/build_android.sh release
platforms/android/gen/android/gradlew -p platforms/android/gen/android :app:assembleUniversalRelease
```

四个 ABI 的原生库会复制到生成目录，再由 Gradle 打包。`prepare_gradle.py` 根据 Cargo 依赖解析生成 Gradle 模块路径和版本信息；`build_android.sh` 自动生成 Tauri/Wry 的 Kotlin 桥接与 ProGuard 文件，新克隆仓库无需复制开发者本机生成文件。

发布签名通过环境变量提供：

```bash
export MAUSFER_KEYSTORE=/private/path/release.keystore
export MAUSFER_KEY_ALIAS=your-key-alias
# 通过本机私有环境配置或 CI Secret 设置：
# MAUSFER_STORE_PASSWORD
# MAUSFER_KEY_PASSWORD
```

未提供签名文件时产出未签名 release APK；可自行签名，或使用 debug 构建调试。签名文件、密码、`local.properties`、缓存和构建产物不应提交。更新已安装 APK 需要沿用相同的签名。

## Java 信令服务

```bash
cd signaling-server
mvn clean package
java -jar target/mausfer-signaling.jar
```

部署见[服务说明](signaling-server/README.md)。IntelliJ IDEA 直接打开 `signaling-server/pom.xml` 作为 Maven 项目即可。

## 提交规范

保持改动聚焦，说明问题、行为变化和验证范围。涉及传输协议时验证接收端最终文件、长度、SHA-256 与完成确认；仅构建通过不能代替真实传输验证。不要提交测试文件、临时日志、私有配置、签名文件或发行包。发行包应放到发布页面。

源码内的自动化回归测试应保留。GitHub Actions 检查 Rust 核心测试、格式与 Clippy，并构建 Java 服务；这些检查不等同于三端真机或跨网络验收。

## 本地缓存与发行

`target/`、Cargo registry/git、Rust 工具链、Gradle caches/wrapper、Maven 本地仓库及 Windows SDK 交叉编译缓存可以保留在本机，均不进入 Git。清理工作区时不要使用 `git clean -xfd`，它会删除这些构建缓存。

发布前核对客户端与服务端版本、APK 签名、macOS 签名和文件 SHA-256；在 Release 上传发行包及校验清单。发布者使用自己的签名凭据，不把私钥和密码写入工作流、源码或 Git 历史。
