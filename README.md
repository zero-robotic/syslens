# Syslens

Rust 编写的 Linux 进程与系统监控程序，支持 NVIDIA Jetson 集成 GPU。采集结果使用 Protobuf 编码，通过 Foxglove Rust SDK 实时发送，并独立写入 MCAP 文件。

一个 Cargo package、一个二进制 crate，通过模块实现分层。

## 构建与运行

需要 Linux、Rust 1.89 或更新版本及 C/C++ 构建工具。Protobuf 编译器由构建依赖提供，无需另外安装 protoc。在目标 Jetson 上构建可避免不同 JetPack 系统的运行库兼容问题。

```sh
cargo build --release --locked

# 默认加载 config/syslens.toml，每秒采集一次，启用 Jetson GPU、实时发送和录制。
./target/release/syslens

# 监控指定进程，允许其他主机连接实时数据。
./target/release/syslens --pid 1234,5678 --interval-ms 500 --bind 0.0.0.0

# 普通 Linux 主机：禁用 Jetson GPU，只录制 60 个采样批次。
./target/release/syslens --no-gpu --no-live --samples 60 --output-dir recordings

# 指定其他配置文件；命令行参数覆盖对应配置项。
./target/release/syslens --config /path/to/syslens.toml
./target/release/syslens --help
```

默认配置文件为 [config/syslens.toml](config/syslens.toml)。未指定 `--config` 时，程序读取当前工作目录下的这个文件；文件缺失、无法读取或格式错误时启动失败并报告原因。从其他目录启动时，可用 `--config` 指定配置文件的绝对路径。配置中的相对路径（例如录制目录）也以当前工作目录为基准。

配置优先级为：命令行参数 > 配置文件 > 未填写字段的内置默认值。修改配置后重新启动生效。

PID 和进程名选择器按“或”匹配；名称精确匹配 `/proc/PID/stat` 的 `comm`。两个选择器都为空时监控所有可见进程。`--no-threads` 可关闭线程扫描；配置中 `processes.enabled = false` 可关闭进程及线程采集。

默认监听 `127.0.0.1:8765`，录制写入 `recordings/`。按 Ctrl+C 或发送 SIGTERM 后，程序完成当前批次、停止采集、排空输出队列并结束 MCAP 文件。录制与实时发送可分别关闭，但至少保留一个输出。

## Foxglove 实时曲线与回放

在 Jetson 运行时指定 `--bind 0.0.0.0`，在另一台主机的 Foxglove 中选择 **Open connection → Foxglove WebSocket**，连接 `ws://JETSON_IP:8765`。这是 SDK 提供的服务，需使用兼容当前 Foxglove SDK 的客户端。[连接说明](https://docs.foxglove.dev/docs/getting-started/rust)

添加 Plot 面板，将下列路径作为曲线的 Y 值，X 轴使用消息时间戳：

| 指标 | 路径示例 |
| --- | --- |
| 系统 CPU 利用率 | `/syslens/system.cpu.usage_percent` |
| 逻辑核 0 利用率 | `/syslens/system.cpu.cores[:]{core_id==0}.usage_percent` |
| 系统已用内存（字节） | `/syslens/system.memory.used_bytes` |
| 系统总内存（字节） | `/syslens/system.memory.total_bytes` |
| PID 1234 的 CPU 利用率 | `/syslens/processes.processes[:]{pid==1234}.cpu_percent` |
| PID 1234 的 RSS（字节） | `/syslens/processes.processes[:]{pid==1234}.rss_bytes` |
| PID 1234 中 TID 1235 的 CPU 利用率 | `/syslens/processes.processes[:]{pid==1234}.threads[:]{tid==1235}.cpu_percent` |
| Jetson GPU 利用率 | `/syslens/gpu.utilization_percent` |
| GPU 共享内存池的系统已用内存 | `/syslens/gpu.shared_system_memory.used_bytes` |

替换示例 PID/TID 后即可选择单个实体；长期跟踪可用 `instance_id` 筛选，避免 PID 复用把两个进程连成一条曲线。数组筛选语法见 [FoxQL](https://docs.foxglove.dev/docs/visualization/foxql)。

回放时直接在 Foxglove 打开录制的 `.mcap` 文件，沿用这些 Topic 和字段路径。每个分段包含完整 Protobuf descriptor（包括被引用的 Timestamp 定义），无需手动提供 `.proto` 文件。

## 指标口径与 Jetson 支持

| 数据 | 来源与口径 |
| --- | --- |
| 系统与逻辑核 CPU | `/proc/stat` 的累计 tick 差分，范围 0–100%；iowait 计入空闲，guest 不重复累计。 |
| 系统内存 | `/proc/meminfo`，输出总量、可用量、已用量与 swap；已用量为 MemTotal − MemAvailable。 |
| 进程 CPU | `/proc/PID/stat` 的 utime + stime 差分，除以实际单调时钟间隔；占满一个逻辑核为 100%，多线程进程可超过 100%。 |
| 进程内存 | RSS 页数乘系统页大小，同时输出虚拟内存字节数。RSS 是内核提供的近似常驻量，包含共享页，不是 PSS。 |
| 线程 CPU | `/proc/PID/task/TID/stat`，与进程 CPU 使用相同口径，分别维护基线。 |
| Jetson GPU 利用率 | 优先读取已知 sysfs GPU `load` 节点（0–1000 转为 0–100%），否则启动一个持续运行的 `tegrastats` 子进程并读取 GR3D_FREQ 百分比。 |
| Jetson GPU 内存 | 输出 GPU 与 CPU 共享的系统 RAM 总量、可用量与已用量；当前不提供 GPU 单独分配量。 |

Jetson 集成 GPU 与 CPU 共享物理 RAM，因此 `shared_system_memory.used_bytes` 表示整个系统占用，不能解释为“GPU 已用显存”。`dedicated_memory_*` 字段保持缺失，不填入虚假的零或系统占用。[NVIDIA 内存说明](https://docs.nvidia.com/cuda/archive/12.8.1/cuda-for-tegra-appnote/index.html)

GPU 节点随平台与 JetPack 版本变化，可通过 `gpu.load_path` 显式指定。未找到节点时，需让 `tegrastats` 在 PATH 内或配置其完整路径。解析支持单频率和 Orin/Thor 多 GPC 频率数组；只有频率、没有百分比的输出视为采集失败。[tegrastats 格式说明](https://docs.nvidia.com/jetson/archives/r38.4/DeveloperGuide/AT/JetsonLinuxDevelopmentTools/TegrastatsUtility.html)

tegrastats 每 100 ms 输出一次，采集时丢弃缓存并等待新行，默认超时 1500 ms；退出时只终止本程序创建的子进程。找到了 sysfs 节点但无法读取时会报告错误，可修正路径或权限。GPU 失败不会停止 CPU、内存等采集。

CPU 利用率第一条样本、首次出现的进程/线程/逻辑核以及计数器重置后没有有效差分，字段保持缺失。进程与线程标识包含系统 boot ID、PID/TID 和启动 tick；退出、PID 复用、核上下线不会沿用旧实体的基线。无法读取的进程会跳过并计数，线程不完整情况写入 `threads_error`。

## 分层与数据流

```text
CPU Collector -------+
                     +--> System Assembler ----> /syslens/system
Memory Collector ----+
          |
          +----------+
                     +--> GPU Assembler -------> /syslens/gpu
Jetson Collector ----+

Process Collector ---+
                     +--> Process Assembler ---> /syslens/processes
Thread Collector ----+

Assembler -> Protobuf 编码一次 -> Dispatcher
                                  |
                                  +-> 独立有界队列 -> MCAP Output
                                  |
                                  +-> 独立有界队列 -> Foxglove Output
```

| 模块 | 职责与依赖边界 |
| --- | --- |
| `app` / `config` | 配置、信号处理、创建并注册所有具体组件。 |
| `framework` | 通用 Collector、Assembler、Output 接口；调度、批次、类型化 Key、队列、生命周期。不了解具体指标与 Protobuf 类型。 |
| `data` | 采集器输出的普通 Rust 数据结构，独立于协议。 |
| `collectors` | 读取与计算指标、维护差分基线；不依赖 Protobuf、组装器或输出实现。 |
| `assemblers` | 声明输入依赖，读取 Batch，组合一个或多个采集结果；不调用具体采集器或输出模块。 |
| `protocol` / `proto` | Protobuf 定义、生成类型、descriptor、编码封装。 |
| `outputs::mcap` | 使用 mcap crate 保存通用消息，管理压缩、分段与结束处理。 |
| `outputs::foxglove` | 使用 Foxglove Rust SDK 发布同一份已编码消息。 |

采集器每个采样批次调用一次，分别在自己的工作线程执行；组装前等待本批所有采集器完成。同一个采集结果可被多个组装器复用，CPU＋内存、进程＋线程、GPU＋内存已经体现了这种组合。

当前使用统一采样周期。超出周期会跳过赶工，不堆积采样任务；慢采集器会延长整个批次。调度和 CPU 差分使用单调时钟，消息采用 Unix 纳秒时间戳。批次时间表示采集开始；`header.inputs` 保存各采集器调用开始时间和错误；GPU 的 `source_timestamp` 保存实际读取/接收时间。这些时间不意味着各指标在同一瞬间取得。

每批创建新的 Batch，失败输入不会使用上一批数据补齐。现有组装器输出可用部分与输入错误状态，利用率缺失和真实的 0% 可区分。程序错误（例如重复注册、依赖未注册、相同 Topic 冲突 Schema）会明确报错。

## 录制与输出行为

- 录制队列满时对采集施加背压，已进入流程的消息不会因队列满而丢弃；磁盘写入错误会使程序报错退出。
- 实时队列满时丢弃新到达的实时消息，并在退出时报告数量。实时发送为尽力而为，慢客户端还受 SDK 内部队列约束。
- 两种输出共享 Protobuf 编码后的字节；各有工作线程和队列，互不调用。
- MCAP 使用 Zstd 压缩，默认每 300 秒或累计 64 MiB 未压缩消息负载后，在下一条消息前轮转；后者不是精确文件大小上限。
- 持续写入时每 1000 ms 完成并 flush 当前 chunk，分段结束和正常退出时写入索引、footer 并同步文件。强制终止或掉电不能保证最后一段完整。
- 队列容量按消息条数限制；大量进程/线程会使单条进程消息变大，可用 PID/名称筛选降低开销。

## 扩展采集能力

新增指标时：

1. 在 `data` 定义普通 Rust 数据，在 `collectors` 实现 `Collector<Data = YourData>`。
2. 在 `app` 创建 `Key<YourData>`，用 `Runtime::add_collector` 注册；框架内部不需要增加指标枚举或分支。
3. 让现有组装器增加该输入，或新增组装器；通过 `dependencies()` 声明 Key 名称，再通过 `batch.get(key)` 读取本批结果。
4. 按需扩展 `proto/syslens.proto`，由组装器构造消息并调用 `protocol::encode`。同一个采集结果可以供多个组装器使用。
5. 在 `app` 注册组装器。两个输出模块无需修改。

Protobuf 已发布字段的编号不能重用，删除字段时保留编号和名称；新增指标优先增加字段或消息。接口与完整消息定义分别见 [framework](src/framework) 和 [syslens.proto](proto/syslens.proto)。

## 验证

在 Linux 执行：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试覆盖计数器差分、核上下线、PID 复用、Jetson 输出解析、组装与采集失败、有界队列与关闭、MCAP 分段及 Schema、真实 Linux 进程/线程到 MCAP，以及真实 WebSocket 订阅后的 Protobuf 和时间戳验证。

开发验证环境为 ARM64 Linux 容器。GPU 测试使用 sysfs/tegrastats 格式样本，尚未在实体 Jetson 上验证驱动节点与权限。
