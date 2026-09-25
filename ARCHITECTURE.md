# Krystallos 架构

Krystallos 是一个协议无关的存储访问内核，用 Rust 编写。它把远程存储呈现为一套类似本地文件系统的接口，让上层调用方可以对 SMB 共享、本地目录、以及将来的 SFTP / WebDAV / NFS / FTP 使用同一套代码。它是 Android 局域网媒体播放器 **HyalosPlayer** 的网络能力底座，但**不依赖 Android**——所有功能都能在桌面主机上通过 CLI 完整驱动和测试。

阅读顺序：职责边界 → 抽象层 → SMB 后端 → 构建链 → FFI 门面 → 预读缓存 → 前向约束 → 风险与逃生口。本文只写决策与理由；实现细节在各 crate 的 doc comment 里，正文只在必要时给出一句结论。

## 设计目标与边界

**做什么**：连接管理、目录列举、随机读写、协议抽象。

**不做什么**：解码、渲染、容器解析、字幕处理。播放器负责这些。这条边界意味着 Krystallos 可以在没有播放器的情况下被完整测试，也是 `krystallos-cli` 能够存在的前提。

### 「像本地文件」这个隐喻的边界

**API 形状像本地是可行的，语义等价于本地则不成立。** 三处差异被显式建模，而不是被掩盖：

| 「像本地」会骗人的地方 | 处理方式 |
|---|---|
| **延迟差三到四个数量级**（本地读微秒级，局域网 SMB 毫秒级） | 每个操作都是 `async`。`std::fs` 那样的阻塞签名是谎言。 |
| **逐项 `stat` 是 N+1 往返灾难** | `StorageBackend::list` 返回的 `Entry` **自带 `Metadata`**，因为多数协议在一次目录枚举里就把两者一起返回了。浏览 1000 项的目录不该产生 1001 次往返。 |
| **网络故障是常态而非异常** | `Error::ConnectionLost` 与 `Error::Io` 是分开的：前者意味着会话已死、后续操作必然也失败，后者是单次操作失败而会话仍可用。 |

### 能力差异是声明出来的，不是撞出来的

协议之间能力并不对等。FTP **没有 positioned read**——随机访问意味着每次 `RETR` 前重发 `REST`，正确但慢得离谱。WebDAV 没有标准的部分写。若接口假设所有协议都能高效随机写，FTP 实现时只能撒谎或者在传输中途失败。

因此 `StorageBackend::capabilities()` 提前声明这些缺口，让调用方能换策略。核心 trait 只取**所有协议都能实现的最小公共集**；协议特有特性（SMB 的 `CHANGE_NOTIFY`、DFS、服务端复制）走独立的可选 trait，不污染核心契约。

## 分层结构

```
krystallos-ffi（对外的唯一门面）    krystallos-cli（主机端调试 CLI）
        └───────────────┬───────────────────┘
                 krystallos-core
   StorageBackend · FileHandle · Capabilities · VfsPath · Entry · Metadata · Error
              ┌─────────┴─────────┐
     krystallos-local      krystallos-smb（actor 线程 + StorageBackend 实现）
      std::fs 后端              └── krystallos-sys-smb2（libsmb2 的 FFI 声明与构建）
                                    └── libsmb2 (C)：git submodule 锁定 commit，静态链入
```

### 为什么要有 `krystallos-local`

不是为了「让播放器读本地文件」，而是两个具体目的：

1. **它是「像本地文件系统」这个隐喻唯一字面成立的地方。** 只对 SMB 有意义的概念在这里都无法实现，所以它是让抽象层保持诚实的机制：若 `krystallos-core` 里出现了 share、NTSTATUS 或线路格式标志，这个 crate 就写不出来。
2. **它是差分测试的基准。** 同一个文件分别经 `krystallos-local` 与 `krystallos-smb` 读出，字节必须一致。这一次比对同时验证了「抽象层没有泄漏」和「SMB 实现正确」——比拿 SMB 自己测自己强得多。

它现在还有第三个作用：**让 CLI 与单元测试有一个不需要网络、不需要共享、不需要账号的靶子。**

## 抽象层设计

### 必须挡住的 SMB 语义泄漏

| SMB 概念 | 处理 |
|---|---|
| **share** | SMB 特有（WebDAV、SFTP 都没有）。由后端自己解析 endpoint URI（`smb://user@host/share/path`），`krystallos-core` 不认识这个词。 |
| **`smb2_stat_64` 的 `smb2_attributes` / `smb2_reparse_tag`** | 归一化成可移植字段，不透出。 |
| **NTSTATUS** | 映射进归一化 `Error` 枚举。libsmb2 同时提供 `-errno`、NTSTATUS 与错误字符串三套，正好够做映射。 |

### 为什么接口是 `read_at` 而不是流式 `Read`

三个独立约束指向同一形状：**UniFFI 没有一等的流类型**（上游 issue #1485 至今开放），数据面只能是按偏移量轮询；**libsmb2 原生就是 pread**（`smb2_pread(ctx, fh, buf, count, offset)`，缓冲区由调用方提供，库直接写入）；**Media3 的 `DataSource` 契约也按偏移量**（`DataOffset.position` 告知 seek 位置后重新 `open()`）。

三者的交汇不是巧合——**随机访问是媒体播放的刚需**，而任何为流式设计的接口最终都要在播放器里被改造成支持 seek。

### 关于 `async-trait`

用 `async-trait` 而非原生 AFIT，因为要支持 `Box<dyn StorageBackend>` / `Box<dyn FileHandle>`，而原生 `async fn` in trait 不是 dyn-compatible 的。代价是每次调用多一次 future 装箱（纳秒级），远在毫秒级网络往返之下。

### `VfsPath` 的规范化规则

- 始终以 `/` 开头，`//` 折叠，无 `.` 段
- `..` 在 `new()` 中解析（越出根则**拒绝**而非钳制——那是调用方 bug 或注入尝试，静默改写会掩盖它）
- **`join()` 拒绝 `..` 而不解析**。这是刻意的：`join` 是用服务端返回的文件名或用户输入拼路径的接口，文件名不该能重定向操作方向。`parent().join("..")` 悄悄变成祖父目录，正是删除/重命名时变成数据丢失事故的那类意外。需要向上遍历时，用 `VfsPath::new` 构造完整路径，让 `..` 在调用点可见。

### 关于文件名：编码与规范化

`Entry::name` 是 `String`。这在 SMB（UTF-16）与 Windows 本地（UTF-16）上都无损，但在 Linux 本地文件系统上会丢失非 UTF-8 文件名。**这是一个已知的、有意的取舍**：全面改用 `Vec<u8>`/`OsString` 会显著复杂化 FFI 边界，而本项目面向 Android 与 Windows。若将来需要支持 Linux 上的任意字节文件名，`Entry::name` 与 `VfsPath` 都需要改造。

**统一使用后端返回的原始名称做后续操作**，不要用用户输入或重新拼接的路径去查找。macOS 共享以 NFD 存储文件名，而用户输入通常是 NFC；重新构造名称会在这些服务器上查找失败。规范化是后端的事，抽象层只负责原样传递。

## SMB 后端

### 为什么是 libsmb2

2017 年至今，作者 Ronnie Sahlberg（Samba 团队成员、libnfs 作者），VLC 全平台（含 Android/iOS）多年生产使用，被 Debian / Ubuntu / vcpkg / OpenBSD 打包。**一处需要纠正的常见说法**：「Kodi 和 QEMU 也在用」是 2019 年的旧宣传——核实过，Kodi 用的是 Samba 的 libsmbclient，QEMU 根本没用它。真正的背书是 VLC + 发行版打包 + 维护者背景。

**另一个关键事实：加密不需要 OpenSSL。** libsmb2 自带全部密码学实现（`aes.c` / `aes128ccm.c` / `sha1.c` / `sha256.c` / `hmac.c` / `md4c.c` / `md5.c`），`configure.ac` 里没有任何 OpenSSL/GnuTLS 检查。唯一可选依赖是 MIT Kerberos，Android 上直接关闭。这消掉了整个构建链里最重的一环。

### 必须锁定 commit，不能取 tag

master 活跃；GitHub Releases **0 个**（该仓库从未使用此功能）；最新 tag `libsmb2-6.2`（2024-12-23）落后 master 350 个提交。

**`libsmb2-6.2` 含 CVE-2025-57632**（堆越界写，CVSS 7.5，`smb2_add_iovector` 缺 `SMB2_MAX_VECTORS` 边界检查，恶意服务器可用链式 PDU 触发）。修复在 PR #431，**不在任何 release 中**。

submodule 锁定在 `557e837d3e00636b543f17ba1b9bdf872fa1644d`（2026-09-19），已验证包含：CVE 修复（`5e75eeb`）；2026-09 的内存/生命周期修复（`pdu: respect caller_frees_pdu in the timeout sweep`、`sync: free the sync_cb_data that smb2_open() allocates`、`notify_change` 错误路径释放）；2026-08 的 `Use a CSPRNG for the client challenge`；以及 `Fix heap buffer overflows in the RPC_SID coders`。

**攻击面提示**：本应用会连接用户填写的任意地址，恶意服务器是真实威胁模型，不是理论风险。这是必须锁修复后 commit 的原因。

### 并发模型

libsmb2 **不是线程安全的**——全仓库 `lib/*.c` grep `pthread` 零命中，无任何内部锁。因此一个 `smb2_context` **必须**固定在一个线程上，actor 模型是必需的，不是可选优化。每个会话一个专用 OS 线程，调用方通过 channel 发命令、等回复，context 指针永不离开该线程。

**为什么用阻塞调用而不是手工驱动事件循环**：libsmb2 也提供异步 API 加 `smb2_get_fd` / `smb2_which_events` / `smb2_service`，可以让一个会话同时有多个操作在途——这是最初的计划，放弃它是因为一个实现时才暴露的工程问题：**事件循环阻塞在 `poll` 时无法感知新命令。** 要么往 poll 集合里塞唤醒通道（Unix 用 pipe、Windows 用 loopback socket，多两套平台相关代码），要么用短 poll 超时，后者给每个操作加延迟下限，并让 CPU **永久性地每秒醒来多次**——手机上是持续的耗电代价。阻塞调用两个问题都没有：空闲时线程零成本停在 `recv`，命令到达即刻唤醒；代价是单会话串行执行操作，而对播放单个媒体流的播放器来说本来就是这样。对外 API 完全相同，若将来需要单会话内并发多个读，改动只限于 `krystallos-smb/src/actor.rs`。

### 文件路径约定

两条来自源码、不能靠猜的事实：**路径相对于共享根，且不带前导分隔符**（`lib/init.c:276-287` 把 `smb2://server/share/dir/file` 解析成 share `share` 与 path `dir/file`）；**共享根是空字符串**（`lib/smb2-cmd-create.c:86` 把 null 或空名视为「无名」，这正是 SMB 寻址树根的方式）。

### Windows 上必须先初始化 Winsock

libsmb2 **不调用 `WSAStartup`**——`lib/socket.c:1464` 只是报错说没初始化。这是库的正确行为：进程级 socket 初始化属于应用而非库。`krystallos-sys-smb2` 通过 `OnceLock` 做一次惰性初始化，其他平台是空操作。

### 错误分类：三个通道，缺一不可

这是实现中最费周折的部分。**libsmb2 不通过单一通道报告失败**，哪个通道有值取决于哪条代码路径失败了。三者都是对着真实服务器观察到的，不是推测：

| 通道 | 覆盖范围 | 出处 |
|---|---|---|
| `smb2_get_nterror()` | 大多数路径 | `smb2_set_nterror`，见 `libsmb2.c:3012`、`:3354`、`:3452`、`:3785` |
| **消息里的 `STATUS_*` 标识** | Create 路径等 | `libsmb2.c:2092` 用的是 `smb2_set_error`，**从不设置 nterror** |
| **返回值 `-errno`** | 消息为空的路径 | `stat` 一个不存在的文件时消息为空、nterror 为 0，只剩返回值 |

因此 `error::from_parts` 依次尝试三者。三个必须照做的约束（详细推导见 `crates/krystallos-smb/src/error.rs`）：

1. **`nterror` 不会被每次调用重置**，可能残留上一次操作的值，分类失败时必须继续往下走，不能直接采信。
2. **裸 `-1` 不是 `-errno`**：`lib/sync.c:76-95` 在 poll 失败、超时无连接或 `smb2_service` 报不可恢复时返回裸 `-1`，三种都意味着连接已断。**绝不能把它喂给 errno 表**（那里 `-1` 与 `-EPERM` 无法区分）——这正是曾经把一个不可达的服务器报成 `permission denied` 的原因，而这是所有可能答案里最误导的一个。
3. **`EPERM` 也不能当作权限错误**：libsmb2 把若干互不相关的状态映到它上面（`lib/errors.c:1123-1128`）。`EACCES` 才是无歧义的权限信号。

另外 `Auth` 的消息不直接用 libsmb2 的文本：凭证被拒后它的描述往往是**次生症状**（socket 被拆掉），裸着显示会让人去排查网络而不是密码。原始文本保留，但明确标为次要。

### 服务端复制

`StorageBackend::copy` **让服务器在它自己的两个句柄之间搬数据**，字节不经过本进程。SMB 为此提供了文件系统控制：源句柄取一个 *resume key*，再向目标句柄发 COPYCHUNK。对一部几 GB 的影片，这是「几秒」与「几分钟」的区别——客户端循环意味着数据在网络上走两遍。

**ctl_code 必须用 `FSCTL_SRV_COPYCHUNK_WRITE`，不是 `FSCTL_SRV_COPYCHUNK`。** 两个变体不是同义词：Samba 对后者要求目标句柄带 `FILE_READ_DATA`，而以 `O_WRONLY` 打开的句柄永远没有——libsmb2 为它请求的是 `FILE_WRITE_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES`，仅此而已。用错变体的症状是 `STATUS_ACCESS_DENIED`，且**客户端看不到这个状态**：libsmb2 在解析该错误回复时会失败（`Unexpected size of Error reply. Expected 9, got 8`）并把整个会话判死，于是错误表现为 `ConnectionLost`。是 Samba 自己的日志给出了真正的原因（`fsctl_srv_copychunk_vfs_done: copy chunk failed [NT_STATUS_ACCESS_DENIED]`）。

**服务器不支持时回退到 `pread`/`pwrite` 流式**，回退留在内核内、对上层不可见：调用方要的是「复制」，不是「用某种方式复制」。判定抽成纯函数 `is_copy_unsupported` 并单独测试——漏掉一个状态码会让老服务器上的复制整体失败而不是降级。设置 `KRYSTALLOS_DEBUG` 时回退会打印一行，因为两条路径的代价差着数量级，而结果本身看不出走的哪条。

**只复制单个文件**，递归是调用方的事，与 `remove_dir` 拒绝非空目录同一条原则。**目标已存在时报错而不覆盖**：替换掉一部影片是调用方必须显式做的决定。

### 加密能力上限

libsmb2 只支持 **AES-128-CCM 加密** 与 **HMAC-SHA256 签名**。`smb2.h` 里定义了 `SMB2_ENCRYPTION_AES_128_GCM`，但 NEGOTIATE 请求里**从不发送它**，`smb3-seal.c` 也从不使用它——常量是死的，只看头文件会误判为支持。

风险场景：服务器被加固成「仅允许 AES-256-GCM」时协商失败。家用 NAS 少见，企业加固环境可能出现。

### 加密的代价：约 96 倍吞吐，因此默认关闭

**自带密码学是构建期的收益，也是运行期的代价。** `lib/aes.c:24-35` 显示，除 Apple 外所有平台（**包括 Android**）都回退到 `aes_reference.c`——一个 469 行的教科书式实现，逐字节 S 盒查表、无 T 表。

对真实 Windows 共享的实测（同一文件、同一环回连接、50 MiB）：

| 配置 | 吞吐 | 每次 1 MiB 读 |
|---|---|---|
| 加密关闭 | **280.56 MiB/s** | 3.56 ms |
| 加密开启 | 2.92 MiB/s | 342.77 ms |

两者读出的数据与本地基准 SHA-256 完全一致。对照之下，Windows 自带 SMB 客户端在同一文件上是 307–504 MB/s——**加密关闭时我们与它同量级，说明栈本身没有问题，差距全部来自加密。**

排查时先怀疑「每次请求的固定开销」和「往返次数太多」，两者都被实测否掉：缓冲从 1 MiB 提到 8 MiB 后耗时按大小成比例增长（256 ms → 1966 ms），说明是逐字节成本；实测就是 50 次 1 MiB 读，且协商出的 `max_read` 有 8 MiB。**决定性的一次测量是 CPU 占用 97.6%**，这排除了「在等定时器或网络」，把方向锁定在 CPU 密集的逐字节处理上；本地后端读同一文件是 1728 MiB/s，排除了我们自己的 Rust 层。

**因此 `smb.seal` 默认关闭，但保留为可选项**（CLI 上 `--smb-seal`，环境变量 `KRYSTALLOS_SMB_SEAL`）。在不受信任的网络上，慢一点总比明文好，那时打开它就是正确的取舍。这不是把安全问题藏起来，而是一个有实测数据支撑的、可翻转的默认值。

**一处遗留问题**：签名也可能是成本的一部分。`init.c:126` 显示 libsmb2 **默认开启签名**，而 Windows 也强制要求，所以那部分开销很可能无法避免——上面的对照只隔离了加密。

## 构建链

### 为什么是 CMake 而不是 `cc`

第一版用 `cc` crate 直接驱动编译器，从 `lib/CMakeLists.txt` 抄源文件列表。这在 Android/Linux 上可行（已知的 Android 移植都这么做），但在 Windows 上不行：**libsmb2 的 `HAVE_*` 特性宏不只是开关可选功能**，有些 `#else` 分支是无守卫的 POSIX 头文件包含（`#ifdef HAVE_FCNTL_H` 走 `#include <fcntl.h>`，否则 `#include <sys/fcntl.h>`——Windows 上不存在）。

Linux 上 `<sys/fcntl.h>` 存在，所以把所有 `HAVE_*` 留空也能碰巧编译通过。MSVC 上不行。手工补齐这些宏等于重新实现 `configure`。

CMake 是上游对两个目标都支持的路径，并且用上游自己的检查为每个平台生成 `config.h`——平台知识留在上游。**代价**：构建时依赖 CMake。**收益**：不必手工维护一个会静默腐烂的 `config.h`（错误的特性宏产生的是微妙的行为错误，不是编译错误）。

### 需要注意的三个 CMake 细节

这三处都是实际踩过的，改动构建脚本时不要回退：

1. **`CMAKE_POLICY_VERSION_MINIMUM=3.5`**——CMake 4 移除了对 `cmake_minimum_required` 低于 3.5 的支持，而 libsmb2 某些分支仍声明 3.2。
2. **Windows 必须显式加 `/DWIN32 /D_WINDOWS`**——`_WINDOWS` 不来自编译器（MSVC 只定义 `_WIN32`）。CMake 通常通过 `CMAKE_C_FLAGS_INIT` 提供，但 `cmake` crate 会**整体赋值** `CMAKE_C_FLAGS`（为了匹配 Rust 的 CRT），覆盖掉初始化值。缺了它 `compat.h` 不进入 Windows 分支，`<stdint.h>` 不被包含，整个库编译失败。
3. **Android 必须强制 `Ninja` 生成器**——Windows 上 `cmake` crate 默认用 Visual Studio 生成器，那是多配置且仅限本地的，交叉编译时会以 `VCTargetsPath` 错误失败。

### Android 构建

```bash
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -P 29 -o <out>/jniLibs build --release
```

- `-P 29` 必须显式给：cargo-ndk 默认 API 21，低于本项目 minSdk 29。build.rs 会在检测到低于 29 时发出警告。
- 需要 `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android`。
- 需要 NDK（本机为 r30 LTS `30.0.16248370`）。build.rs 按 cargo-ndk 的顺序查找：`ANDROID_NDK_HOME` → `ANDROID_NDK_ROOT` → `ANDROID_NDK_PATH` → `NDK_HOME` → `$ANDROID_HOME/ndk/<最高版本>`。

**16 KB 页对齐**：NDK r28+ 默认对齐，`cargo-ndk` 对 64 位目标自动加链接参数。验证：`llvm-objdump -p libkrystallos_ffi.so | grep LOAD`，64 位应见 `align 2**14`。armeabi-v7a 保持 `2**12`（4 KB）是**正确的**——16 KB 页只适用于 64 位 ABI。

**验证 libsmb2 确实被链入**：release 构建开了 LTO + `--gc-sections` + `strip`，不可达的 libsmb2 代码会被消除，因此 release 的 `.so` 里查不到 smb2 符号**属正常**。要验证链接，用 debug 构建后 `llvm-nm <out>/arm64-v8a/libkrystallos_ffi.so | grep smb2_init_context`。

## FFI 门面

`krystallos-ffi` 是 Android 侧唯一会看到的接口，用 UniFFI 0.32 的 proc-macro 模式声明。

### 三个对象，而非一个

它们有真正不同的生命周期，值得在 Kotlin 侧可见：

| 对象 | 生命周期 | 说明 |
|---|---|---|
| `Kernel` | 每进程一个 | 持有 backend 注册表，`connect` 的入口 |
| `Session` | 一次连接 | 打开即认证，关闭即断开 |
| `RemoteFile` | 依附于 Session | 句柄只在 Session 存活期间有效 |

### 数据面为什么是 async 而不是同步 `readInto(ByteBuffer)`

原计划是同步的 `readInto(offset, ByteBuffer)`，基于「UniFFI 的 `&[u8]` 零拷贝能用在参数上」这个判断。**这个判断有一半是错的**：`&[u8]` 只从外部流向 Rust，且**不存在 `&mut [u8]` 对应物**，UniFFI 无法让 Rust 写进调用方的缓冲区。

所以读操作只能返回 owned bytes，边界上必然有一次拷贝。**一旦拷贝无法避免，同步签名就买不到任何东西，反而丢掉了 await 的能力。** 因此 `read_at` 是 async 的，返回 `ByteRange { offset, data: Vec<u8> }`。

代价小到不值得绕开：100 Mbps 的 4K 流约 12 MiB/s，按 1 MiB 分块是每秒约 12 次拷贝，而它们背后的网络往返以毫秒计。**返回 record 而非裸 `Vec<u8>`** 也留了余地：将来要加字段（比如实际服务的 offset）不必改签名。

### 错误类型为什么独立于 `krystallos_core::Error`

内核的错误模型应该随内核自由演进。**凡是通过 UniFFI 导出的东西都成为已发布 API**——Kotlin 侧已经写好的代码依赖它。直接绑定两者意味着内核的任何重构都是 Android 侧的破坏性变更。

因此边界有自己的 `KernelError`，映射显式且可测试。它也只携带调用方能据以行动的信息：`Io(std::io::Error)` 在 Kotlin 侧没有意义，而 `ConnectionLost` 与 `NotFound` 的区别正是 UI 需要的——一个意味着「提示重连」，另一个意味着「那个文件没了」。

### 绑定生成

UniFFI 没有 Gradle 插件，生成是独立一步，读取编译好的 `.so` 并产出一个自包含的 `.kt`（命令见 [README](README.md#生成-kotlin-绑定)）。用 `cargo run -p krystallos-ffi --bin uniffi-bindgen` 而非全局安装的 `uniffi-bindgen`，**目的是让生成器的版本被 `Cargo.lock` 锁住**，不会与构建库时的 `uniffi` 版本漂移——版本不匹配会产出「能编译、运行时才炸」的绑定。

**Android 侧的两个必备项**（已查证，不是可选项）：

1. JNA 依赖必须用 **aar** 变体：`net.java.dev.jna:jna:<ver>@aar`。用普通 jar 会在运行时 `UnsatisfiedLinkError`（找不到 `libjnidispatch.so`）。
2. 若开了 minify，加 `-keep class com.sun.jna.** { *; }`。

有 async 函数时还需要 `kotlinx-coroutines-core`。

## 预读缓存

`krystallos-cache` 提供 `ReadAhead<H>`，一个包在任意 `FileHandle` 外面的装饰器。它改变的是**延迟**，不是吞吐。

实测数据（真实共享、环回连接、50 MiB 文件、64 KiB 读块——**模拟 Media3 的读法**）：

| | 往返次数 | 墙钟耗时 |
|---|---|---|
| 无预读 | **801** | 0.12 s |
| 1 MiB 预读 | **51** | 0.11 s |

**往返次数降了 15.7 倍，墙钟几乎没变。** 这个对比本身就是最重要的一课：**在低延迟链路上，墙钟时间根本测不出预读的价值**——800 次往返总共才 0.12 秒，字节传输比往返开销大得多。只看耗时就会得出「这层没用」的结论，而它实际上把往返次数降了一个数量级。真正受益的是高延迟链路（真实 Wi-Fi、跨网段、公网），那时每次往返以几十毫秒计，800 次就是几十秒。

因此**测试用往返次数而非耗时作为判据**（`crates/krystallos-cache/tests/live_readahead.rs`），`ReadAhead` 也暴露 `inner_read_count()` 让调用方能自己验证。

### 设计约束

`FileHandle` 取 `&self` 而非 `&mut self`。多个任务可能同时读一个句柄——播放器确实会这样：解复用器在读前缓冲，同时播放器在 seek。所以窗口不能放在普通 `&mut` 后面，也**不能是跨 await 持有的锁**——那会把整个文件的所有读串行在正在取数据的那一个后面。

采用的办法是：窗口放在 `std::sync::Mutex` 里，**锁从不跨 await 持有**。读操作只在两件事上持锁：从窗口拷出字节，或判断窗口没覆盖所需范围。取数据在锁外进行，结果在锁内发布。两个并发读者因此可能都未命中、都去取同一段，后发布的胜出、先取的被丢弃。**这是浪费的工作，不是错误的工作**，而且远比单飞（single-flight）协调简单——后者的收益只在并发随机读时才显现，而那正是这层不为它而存在的访问模式。

### 三个容易写错的地方

1. **窗口必须对齐到窗口边界，而不是从请求偏移开始。** 否则一个每次读 1 MiB 的调用方会依次请求 0、1 MiB、2 MiB……每次都是新往返，等于没有预读——与目的完全相反。
2. **请求大于窗口时必须直接透传。** 单个窗口装不下跨窗口的请求，取一个窗口也帮不上忙；交给底层句柄（它内部会分块）才是对的。写错的话会静默截断数据。
3. **短读不是 EOF，只有读回 0 字节才是。** libsmb2 的 `smb2_pread` 会把请求静默缩到已授予 credits 能覆盖的大小（每个 credit 64 KiB；SMB 2.0.2 固定 64 KiB 上限，见 `lib/libsmb2.c` 的 `smb2_pread_async`），所以文件中途的短读是常态。曾经 `fill()` 把「读回少于请求」当作 EOF，一旦 credits 不足，窗口之后的内容全部读成 0 字节——相当于影片被截断。现在 `fill()` 循环直到填满或读回 0；FFI 的 `RemoteFile::read_at` 同理循环，以兑现「短结果即 EOF」对 Kotlin 侧的承诺。在环回地址上对 Windows 共享的实测里 credits 始终充足，所以集成测试没有暴露这个问题，回归由 `a_short_read_mid_file_is_not_mistaken_for_end_of_file` 这类单元测试守住。

### 一次实测发现的边界行为

**「文件已到末尾」无法从一次成功的读里推断出来**：请求 4096 字节拿回 4096 字节，与「文件正好在这里结束」无法区分。所以**第一次读到文件末尾之后的探测往返无法避免**，之后才能记住。这一点写进了测试的断言里，而不是假装能省掉。

### 与 CLI 的关系

`--read-ahead` 与 `--chunk` 是两个独立开关，因为**它们的交互就是全部故事**：预读只在调用方读得比窗口小时才有用。默认 `--chunk 65536` 接近播放器的读法，而不是窗口大小。

## 前向约束

以下约束来自完整的选型调研，**当前尚未实现**，但 Krystallos 的接口形状已经为它们定好了。改动抽象层前请先读这一节。

### HyalosPlayer 侧

- **播放引擎：Media3 1.11.x**（Apache-2.0）。不用 libVLC（其 SMB 支持仅到 SMBv1，基于 libdsm）。
- **数据面桥接：UniFFI + 直接 `DataSource`**，不用「Rust 内起本地 HTTP server」。理由：少一层 IPC、无端口/鉴权/后台被杀风险。
- **`DataSource` 内部必须做 MB 级预读缓冲，绝不能把调用方的 `readLength` 透传到 SMB 层。** 这是 SMB 场景的头号性能杀手：社区实测有自定义 DataSource 被以 `readLength == 1` 连续调用 60 万次以上，初始化耗时数分钟。
- **UniFFI 不支持取消**。Kotlin 的 `Job.cancel()` 不会传到 Rust。目前可接受，因为每个操作都受 libsmb2 的超时约束（`DEFAULT_TIMEOUT_SECS`），但 UI 上的「取消」实际含义是「不再等待」而非「停止工作」。

### 已确认的产品决策

- **不做 ASS/SSA 特效字幕**。Media3 不使用 libass，且 Google 工程师已明确表态「持续不考虑对用户生成内容使用 native code」。这是产品级取舍，不是能靠工程绕过的。因此也**不引入 libmpv 副引擎**（它虽能解决字幕，但在 Android 上无法输出 HDR，且需自维护四个 ABI 的 `.so`）。
- **目标设备为手机/平板**，不做 Android TV——因此无焦点导航与 Leanback 需求。
- **字幕功能整体不在计划内**。

### 许可证

两个仓库均为 **GPL-3.0**。选 3.0 而非 2.0 有两个硬理由：libsmb2 是 **LGPL-2.1-or-later**（可选 LGPL-3.0 条款，与 GPL-3.0 兼容，与 GPL-2.0-only 不兼容）；将来的 Media3 是 Apache-2.0（与 GPL-3.0 兼容，与 GPL-2.0 不兼容）。

## 已知风险

| 风险 | 说明与应对 |
|---|---|
| **libsmb2 无 Release 流程** | 将来不会有安全更新通知，只能自己盯 master 提交。定期检查 submodule 上游是有必要的维护动作。 |
| **bus factor = 1** | 单一维护者。靠 `krystallos-sys-smb2` 的适配器隔离对冲，见「逃生口」。 |
| **加密仅 AES-128-CCM** | 见上文。已确认接受此上限。 |
| **加密开启后吞吐降至约 3 MB/s** | 见上文。默认关闭，按连接可选开启。 |
| **Windows 主机端 CSPRNG 偏弱** | Windows 上没有 `arc4random_buf` / `getrandom` / `/dev/urandom`，libsmb2 回退到 `random()`。仅影响调试 CLI（不对外分发），Android 侧走 `arc4random_buf`（API 21+）是强随机的。**若 Windows 端将来不只是开发便利，需要重新评估。** |
| **`Entry::name` 是 `String`** | 非 UTF-8 文件名在 Linux 本地后端会丢失。见上文「关于文件名：编码与规范化」。 |

## 逃生口

**换后端是一次局部新增，不是全项目重构。** 这由一条规则保证：

> **`krystallos-sys-smb2` 的类型不得出现在 `krystallos-core` 的任何签名里。**

若因加密上限、维护断档或性能问题需要换掉 libsmb2，替代方案是：

- **`smb`（afiffon/smb-rs）**——功能更全（NetBIOS / QUIC / RDMA / multichannel / Kerberos），密码套件覆盖更广（AES-128/256 CCM+GCM）。代价是硬依赖 `sspi` + `ring`（C + 汇编），Android 交叉编译需手工处理 `TARGET_CC` / `TARGET_AR`，且 armv7 与 x86_64 未经上游 CI 验证。
- **自研**——最小可用客户端约 5,000–8,000 行（1–2 个月）；生产级 20,000–30,000+ 行（6 个月以上）。难点全在长尾：credit 记账算错会导致吞吐崩塌或断连、SMB 3.1.1 的 preauth integrity hash 链、加密 transform header 与签名的叠加顺序、oplock/lease break 的及时应答。参考实现可读性排序：`smbj`（Java）> `go-smb2`（Go）> `libsmb2`（C）。

**协议扩展**同样是局部新增：实现 `BackendDriver`，交给 `BackendRegistry::register`，该 scheme 即可从所有子命令和 FFI 门面访问。

## 测试策略

| 层次 | 方式 |
|---|---|
| 单元测试 | `cargo test --workspace`：路径规范化、错误映射、凭据处理、URI 解析 |
| 抽象层纯净性 | `krystallos-local` 全套操作在无网络环境跑通；`krystallos-core` 中不得出现任何 SMB 专属概念 |
| **差分测试** | 同一文件分别经 `local` 与 `smb` 后端读出，比对 SHA-256；目录列举结果比对 |
| 集成测试 | 对着真实 SMB2/3 共享：列举、stat、**>1 GB** 文件分块读取校验、建目录→上传→重命名→读回→删除全链路 |
| 交叉编译 | 三个 ABI 产出 `.so`，64 位目标满足 16 KB 对齐 |
| 手工验证 | `krystallos-cli` 对真实共享操作，与 Windows 资源管理器所见交叉比对 |

`crates/krystallos-smb/tests/live_share.rs` 里是对着真实服务器跑的测试，未设置环境变量时**跳过而非失败**（环境变量清单见该文件顶部），所以在没有共享的机器上 `cargo test` 依然是绿的。`KRYSTALLOS_TEST_LOCAL_URI` 指向**同一个目录**时，差分测试会启用——这是本地后端存在的意义所在。

**端到端判据**：`krystallos-cli` 能对 `file://` 与 `smb://` 两种 scheme 执行同一套操作——列举目录、读取 **>1 GB** 文件并通过 SHA-256 差分比对一致、完成上传→重命名→删除的完整往返。
