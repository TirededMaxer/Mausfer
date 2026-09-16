# 构建与开发

## GitHub 云端构建（推荐）

无需在自己的电脑上安装构建工具。打开仓库 **Actions → Cloud Build → Run workflow**：

1. 选择 `main`。留空 `draft_tag` 时只生成构建附件；填写尚未发布的新标签（例如 `v0.1.2`）时，同时生成 Release 草稿。推送 `v*` 标签也会生成草稿。
2. 等待检查与构建任务全部通过。Windows 使用 `windows-2022`，macOS ARM64 使用 `macos-15`，Android 和 Java 服务使用 Ubuntu。Android 四种架构分别编译后合并为通用 APK。
3. 下载本次运行的 `Mausfer-packages` 附件，或草稿中的对应安装包，进行实际设备与文件传输测试。附件同时包含 `SHA256SUMS.txt`，Actions 构建附件保留 7 天。
4. 测试确认后，在 **Actions → Publish Tested Release → Run workflow** 中填写该草稿标签。工作流会校验全部附件的 SHA-256，发布为最新版，再删除其他已发布 Release。其他草稿和 Git 标签保留。构建工作流本身不会正式发布或删除已有 Release。

客户端与信令服务可以使用不同版本，下载文件名读取各自源码版本，Release 草稿说明会列出组件版本、源码提交和构建链接。Windows 同时提供便携 EXE、NSIS 安装包和 MSI；macOS 提供 DMG/ZIP；Android 提供通用 APK；服务端提供 JAR/ZIP 和默认配置。

### 发布签名

Android 云端构建必须配置以下 **Actions repository secrets**，APK 沿用原签名才能覆盖安装旧版：

| Secret | 内容 |
| --- | --- |
| `MAUSFER_ANDROID_KEYSTORE_BASE64` | 原发布 keystore 的 Base64 内容 |
| `MAUSFER_STORE_PASSWORD` | keystore 密码 |
| `MAUSFER_KEY_ALIAS` | 签名别名 |
| `MAUSFER_KEY_PASSWORD` | 签名密钥密码 |

签名文件在打包任务的临时目录中恢复，使用后删除，不进入源码、缓存或构建附件。APK 打包后核对发布证书指纹。维护者更换签名时需同步更新工作流中的预期指纹。

macOS 沿用本地签名，尚未经过 Apple 公证；Windows 未作商业代码签名。云端编译与包校验不代替实际设备或跨网络传输验收。

### 本机目录与空间

全套云端构建验证成功后，本机只需保留源码、Git 数据、自己的私有签名备份及需要测试的安装包。项目内 `target/`、`.cargo/` 中的工具和依赖、`.rustup/`、`.gradle-home/`、`.xwin/` 以及 Android 生成的构建目录可删除；必须保留已跟踪的 `.cargo/config.toml` 和 Android 工程源码。其他项目共用的系统 SDK/JDK 不属于这个项目的清理范围。

以下为需要在本机构建时的可选说明。

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
