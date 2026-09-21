# Krystallos

协议无关的存储访问内核。「Krystallos」是希腊语的冰——Hyalos（玻璃）的原料。

Krystallos 把远程存储呈现为一套类似本地文件系统的接口：连接、列举目录、随机读写。首个支持的协议是 SMB2/3，架构上为 SFTP / WebDAV / NFS / FTP 预留了扩展位。

它是 Android 局域网媒体播放器 [HyalosPlayer](https://github.com/sheepthefather/Hyalos-Player) 的网络能力底座，但**不依赖 Android**——全部功能都能在桌面主机上通过 CLI 驱动和测试，不需要模拟器、不需要真机。

## 当前状态

**内核已完整可用**：抽象层、本地后端、SMB2/3 后端、调试 CLI、UniFFI 门面均已实现并通过测试（含对真实共享的集成测试与后端间差分测试）。

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M1 | 工作区 + libsmb2 构建 + 三 ABI 交叉编译 | ✅ |
| M2 | `krystallos-core` 抽象层 | ✅ |
| M3 | `krystallos-local` + `krystallos-cli` | ✅ |
| M4 | SMB 连接、列举、stat、目录操作 | ✅ |
| M5 | SMB 文件读写 | ✅ |
| M6 | 预读缓存 | ✅ |
| M7 | UniFFI 门面 + Kotlin 绑定生成 | ✅ |

## 快速开始

```bash
git clone --recursive https://github.com/sheepthefather/Krystallos.git
cd Krystallos
cargo test --workspace
```

克隆时**必须带 `--recursive`**：libsmb2 是 submodule。若已克隆但忘记，执行 `git submodule update --init --recursive`。

### 用 CLI 试一下

```bash
# 看这个构建支持哪些协议
cargo run -p krystallos-cli -- schemes

# 列举一个本地目录
cargo run -p krystallos-cli -- ls file:///D:/media

# 上传、下载、改名、删除
cargo run -p krystallos-cli -- put file:///D:/media /movies/a.mkv ./a.mkv
cargo run -p krystallos-cli -- get file:///D:/media /movies/a.mkv ./copy.mkv
cargo run -p krystallos-cli -- mv  file:///D:/media /movies/a.mkv /movies/b.mkv
cargo run -p krystallos-cli -- rm  file:///D:/media /movies/b.mkv
```

密码走环境变量，避免留在 shell 历史里：

```bash
export KRYSTALLOS_USER=b KRYSTALLOS_PASSWORD=...
cargo run -p krystallos-cli -- ls smb://nas.local/media
```

同一个 CLI 对两种 scheme 完全等价，这是它存在的意义——后端行为不必经过模拟器或真机即可验证：

```bash
cargo run -p krystallos-cli -- ls  file:///D:/media
cargo run -p krystallos-cli -- ls  smb://nas.local/media
cargo run -p krystallos-cli -- stat smb://nas.local/media /movies/a.mkv
cargo run -p krystallos-cli -- mkdir smb://nas.local/media /new-folder
cargo run -p krystallos-cli -- mv   smb://nas.local/media /a.mkv /b.mkv
```

### 关于 SMB3 加密

`--smb-seal` 请求 SMB3 传输加密，**默认关闭**。

libsmb2 在除 Apple 外的所有平台（含 Android）都使用自带的参考版 AES 实现，开启加密会让读吞吐从约 280 MB/s 降到约 3 MB/s——实测相差 96 倍。在不受信任的网络上打开它是对的取舍；在自家局域网里默认关闭。

```bash
KRYSTALLOS_PASSWORD=... cargo run -p krystallos-cli -- --smb-seal ls smb://nas.local/media
```

细节与实测数据见 [ARCHITECTURE.md](ARCHITECTURE.md#加密的代价约-96-倍吞吐因此默认关闭)。

## 构建到 Android

需要 NDK 与 cargo-ndk：

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
```

然后：

```bash
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -P 29 \
    -o target/jniLibs build --release -p krystallos-ffi
```

`-P 29` 不要省略——cargo-ndk 默认按 API 21 构建，低于本项目 minSdk。

**构建还需要 CMake 3.x 或 4.x。** libsmb2 用它生成平台相关的 `config.h`，绕不过去；原因见 [ARCHITECTURE.md](ARCHITECTURE.md#为什么是-cmake-而不是-cc)。

### 生成 Kotlin 绑定

UniFFI 没有 Gradle 插件，绑定生成是独立一步，读取编译好的 `.so`：

```bash
cargo run -p krystallos-ffi --bin uniffi-bindgen -- \
    generate --library target/jniLibs/arm64-v8a/libkrystallos_ffi.so \
    --language kotlin --out-dir target/generated/kotlin
```

产出一个自包含的 `krystallos_ffi.kt`。用 `cargo run` 而不是全局安装的 `uniffi-bindgen`，是为了让生成器版本被 `Cargo.lock` 锁住，不会与构建库时的版本漂移——不匹配会产出「能编译、运行时才炸」的绑定。

**Android 工程侧的两个必备项**：JNA 依赖必须用 aar 变体（`net.java.dev.jna:jna:<ver>@aar`），否则运行时 `UnsatisfiedLinkError`；有 async 函数时需要 `kotlinx-coroutines-core`。

## 仓库结构

```
crates/
├── krystallos-core/       协议中立的抽象层
├── krystallos-local/      本地文件系统后端
├── krystallos-smb/        SMB2/3 后端（actor 线程）
├── krystallos-sys-smb2/   libsmb2 的 FFI 声明与构建
├── krystallos-cache/      预读窗口
├── krystallos-ffi/        对外门面（cdylib）+ Kotlin 绑定生成
└── krystallos-cli/        主机端调试 CLI
vendor/libsmb2/            git submodule，锁定 commit
```

## 设计要点

完整的架构说明、决策理由与已知风险见 **[ARCHITECTURE.md](ARCHITECTURE.md)**。几条最需要知道的：

- **「像本地文件」这个隐喻有边界。** 接口形状像本地，语义不等价——延迟差三到四个数量级、逐项 `stat` 会变成 N+1 往返、网络故障是常态。这三处被显式建模而不是被掩盖。
- **`read_at` 而不是流式 `Read`。** 三方约束（UniFFI 无流类型、libsmb2 原生 pread、Media3 DataSource 按偏移量）指向同一形状。
- **能力差异是声明出来的。** FTP 没有 positioned read，WebDAV 没有标准的部分写——接口提前说明，而不是让调用方在传输中途发现。
- **libsmb2 锁定 commit 而非取 tag。** 最新 tag 是 21 个月前的，含 CVE-2025-57632，修复不在任何 release 中。

## 许可证

GPL-3.0-or-later。见 [LICENSE](LICENSE)。

vendored 的 libsmb2 是 LGPL-2.1-or-later，其许可证文本在 `vendor/libsmb2/LICENCE-LGPL-2.1.txt`。
