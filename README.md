# Syslens

Rust 编写的 Linux 进程与系统监控程序，面向 NVIDIA Jetson Orin、Thor 集成 GPU。采集结果使用 Protobuf 编码，通过 Foxglove Rust SDK 实时发送，并独立写入 MCAP 文件。

一个 Cargo package、一个二进制 crate，通过模块实现分层。

## 构建与运行

需要 Linux、Rust 1.89 或更新版本及 C/C++ 构建工具。Protobuf 编译器由构建依赖提供，无需另外安装 protoc。在目标 Jetson 上构建可避免不同 JetPack 系统的运行库兼容问题。

```sh
cargo build --release --locked

# 默认加载 config/syslens.toml，按各指标独立周期采集，每 100 ms 发布消息。
./target/release/syslens

# 监控指定进程，允许其他主机连接实时数据。
./target/release/syslens --pid 1234,5678 --bind 0.0.0.0

# 普通 Linux 主机：禁用 Jetson GPU，只录制 60 个发布批次。
./target/release/syslens --no-gpu --no-live --samples 60 --output-dir recordings

# 指定其他配置文件；命令行参数覆盖对应配置项。
./target/release/syslens --config /path/to/syslens.toml
./target/release/syslens --help
```

默认配置文件为 [config/syslens.toml](config/syslens.toml)。未指定 `--config` 时，程序读取当前工作目录下的这个文件；文件缺失、无法读取或格式错误时启动失败并报告原因。从其他目录启动时，可用 `--config` 指定配置文件的绝对路径。配置中的相对路径（例如录制目录）也以当前工作目录为基准。

配置优先级为：命令行参数 > 配置文件 > 未填写字段的内置默认值。修改配置后重新启动生效。

采集周期集中放在配置文件的 `[sampling]` 中，单位为毫秒：

| 配置项 | 默认值 | 控制范围 |
| --- | ---: | --- |
| `system_cpu_ms` | 100 | 系统总 CPU 和各逻辑核 CPU，共享一次 `/proc/stat` 读取。 |
| `system_memory_ms` | 200 | 系统内存、swap。 |
| `process_cpu_ms` | 100 | 进程列表和进程 CPU。 |
| `process_memory_ms` | 200 | 进程 RSS、虚拟内存，按 PID 和启动 tick 与进程 CPU 结果关联。 |
| `thread_cpu_ms` | 100 | 进程内各线程 CPU。 |
| `gpu_utilization_ms` | 100 | 系统 GPU 利用率；也控制 tegrastats 的输出间隔。 |
| `gpu_memory_ms` | 500 | 系统及进程 GPU 内存，共享驱动内存快照。 |
| `process_gpu_utilization_ms` | 1000 | 进程 GPU 利用率；pmon 限制至少 1000 ms。 |
| `stale_after_periods` | 3 | 输入超过自身采集周期的此倍数后过期，组装时不使用。 |

顶层 `interval_ms = 100` 是**消息发布周期**，`--interval-ms` 只覆盖发布周期，不改变各采集器周期。`--samples` 是发布批次数，首批在一个发布周期后输出，不等待慢采集器完成。采集周期和发布周期允许 1 ms 至 24 小时；这些是目标间隔，实际速度受系统负载和驱动接口约束。缩短采集周期时，应同步缩短发布周期以减少中间样本被覆盖，并观察下述 `skipped_samples`。

例如，排查指定进程时可以设置 `process_cpu_ms = 50`、`thread_cpu_ms = 50`，同时设置顶层 `interval_ms = 25`。高频扫描全部进程与线程会增加开销，建议通过 PID/名称限定目标，并在 Orin/Thor 实机测量采集耗时。配置修改后重启生效。


PID 和进程名选择器按“或”匹配；名称精确匹配 `/proc/PID/stat` 的 `comm`。两个选择器都为空时监控所有可见进程。`--no-threads` 可关闭线程扫描；配置中 `processes.enabled = false` 可关闭进程及线程采集。

默认监听 `127.0.0.1:8765`，录制写入 `recordings/`。按 Ctrl+C 或发送 SIGTERM 后，程序停止发布、唤醒并停止采集线程，等待正在执行的采集结束，排空输出队列并结束 MCAP 文件。录制与实时发送可分别关闭，但至少保留一个输出。

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
| 系统 GPU 利用率 | `/syslens/system.gpu.utilization_percent` |
| 系统 GPU 内存统计（口径见 `memory_scope`） | `/syslens/system.gpu.memory_used_bytes` |
| PID 1234 的 GPU 利用率（接口支持时） | `/syslens/processes.processes[:]{pid==1234}.gpu.utilization_percent` |
| PID 1234 的 GPU 内存统计 | `/syslens/processes.processes[:]{pid==1234}.gpu.memory_used_bytes` |

每个发布批次默认发送两条消息：`SystemSample`（`/syslens/system`）与 `ProcessSample`（`/syslens/processes`，包含所有选中的进程）。关闭进程采集后只有系统消息。不再发布 `/syslens/gpu`；旧 `GpuSample` 定义保留为 deprecated，旧 MCAP 使用文件内的 schema 回放。已有 GPU 曲线路径需要改成上表路径。

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
| 系统 GPU 利用率 | 优先读取已知 sysfs GPU `load` 节点（0–1000 转为 0–100%），否则读取持续运行的 `tegrastats` 的 GR3D_FREQ；Thor 也可由 nvidia-smi 补充。 |
| Orin GPU 内存统计 | `/sys/kernel/debug/nvmap/iovmm/clients` 的内核 total 与各 PID 的 client allocation，按 K/M/G 转换成字节。nvmap 可能包含多媒体、共享分配，不是 GPU 独占物理显存。 |
| Thor GPU 内存统计 | nvidia-smi 的系统 `memory.used`、`memory.total` 和 `compute-apps.used_memory`，MiB 转换成字节；进程内存接口覆盖计算进程。返回 N/A 时字段缺失。 |
| 进程 GPU 利用率 | Orin 常规接口不支持；Thor 尝试 nvidia-smi pmon 的 SM 利用率，驱动支持且返回有效值时才填入。没有进程记录不推断为 0%。 |

Jetson 与 CPU 共享物理 RAM。`SystemSample.memory` 保留系统 RAM 数据，`gpu.memory_used_bytes` 只存对应驱动的分配统计，不用系统内存或进程 RSS 代替。Orin 的 `memory_scope` 明确标注 nvmap 口径，系统值使用内核提供的 total，不把筛选后的进程数值相加；共享分配可能导致各进程统计不能相加。无法查询 GPU 内存总量时 `memory_total_bytes` 缺失，不拿系统 RAM 总量代替。[NVIDIA 内存说明](https://docs.nvidia.com/cuda/archive/12.8.1/cuda-for-tegra-appnote/index.html)、[NVIDIA Orin/Thor 诊断接口示例](https://github.com/NVIDIA/skills/blob/main/skills/jetson-diagnostic/scripts/snapshot.sh)

两代平台使用相同消息结构，按实际接口能力选取来源。Orin 的 nvmap 节点必须有读取权限；可配置 `gpu.nvmap_clients_path`。nvmap 不可用时尝试 `gpu.nvidia_smi`，拒绝 Orin 的 `(nvgpu)` 占位设备。Thor 的系统显存和进程利用率是否可取取决于驱动/BSP，不能保证每个版本都有。[Orin 进程利用率限制](https://forums.developer.nvidia.com/t/how-to-check-gpu-usage-percentage-by-process-on-jetson/319750)、[nvidia-smi 查询说明](https://docs.nvidia.com/deploy/nvidia-smi/index.html)

GPU load 节点随平台与 JetPack 版本变化，可用 `gpu.load_path` 显式指定。未找到节点时，需让 `tegrastats` 在 PATH 内或配置其完整路径。解析支持单频率和 Orin/Thor 多 GPC 数组；只有频率而没有百分比时不生成利用率。[tegrastats 格式说明](https://docs.nvidia.com/jetson/archives/r38.4/DeveloperGuide/AT/JetsonLinuxDevelopmentTools/TegrastatsUtility.html)

启动时分别探测系统利用率、内存统计与可用的进程接口。失败的来源只提示一次，本次运行不再尝试；部分来源失败不影响其他来源。显式报告所有活动进程值均为 N/A 的接口也会停用；空进程表本身不代表失败，允许后续新进程被采集。修复驱动、路径或权限后需要重启。`--no-gpu` 或 `gpu.enabled = false` 直接跳过全部 GPU 探测，消息中的 `gpu` 缺失；启用但不可用时保留空指标及 `utilization_error` / `memory_error`。

探测数据不发送，运行时读取新观测。tegrastats 按 `sampling.gpu_utilization_ms` 持续输出；采集器优先消费最新的未使用观测，丢弃过旧缓存，避免每次都额外等待完整输出周期。探测启动的子进程继续使用。nvidia-smi 使用有超时的只读命令，每次最多接收 4 MiB 输出，默认超时为 `gpu.timeout_ms = 1500`；使用较慢的 tegrastats 输出周期时需相应调整超时。

系统 GPU 利用率、GPU 内存、进程 GPU 利用率分别在独立采集线程运行，pmon 不再阻塞内存或 CPU 采集。GPU 内存采集内部的系统/进程查询仍共用一次采集调用。已启用来源允许在运行时错误后恢复；错误替换该输入的旧结果，不继续使用上一次成功值。

GPU 指标含独立的来源、实际读取时间和缺失原因。进程 GPU 数据在查询前后核对 `/proc/PID/stat` 的启动 tick，组装时再按 `(PID, start_ticks)` 匹配，避免 PID 复用混淆。正常退出时只终止本程序创建的子进程。

CPU 采集器的首次观测、首次出现的进程/线程/逻辑核以及计数器重置后没有有效差分，利用率字段保持缺失。发布周期独立，因此第一条消息可能已包含第二次 CPU 观测。进程内存尚未采集、过期或身份不匹配时，`rss_bytes` / `virtual_bytes` 缺失；这两个字段增加了 optional presence，编号和 wire type 保持兼容。进程与线程标识包含系统 boot ID、PID/TID 和启动 tick；退出、PID 复用、核上下线不会沿用旧实体的基线。无法读取的进程会跳过并计数，线程不完整情况写入 `threads_error`。

## 分层与数据流

```text
CPU Collector -------------------+
Memory Collector ----------------+--> System Assembler ---> /syslens/system
GPU Utilization Collector -------+
GPU Memory Collector ------------+
                                 |
                                 +--> Process Assembler --> /syslens/processes
Process CPU Collector -----------+
Process Memory Collector --------+
Thread CPU Collector ------------+
Process GPU Utilization Collector+

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

协议按消息职责分为三个文件：

| 文件 | 定义 |
| --- | --- |
| [common.proto](proto/common.proto) | `Header`、`InputStatus`、系统和进程共用的 `GpuMetrics`。 |
| [system.proto](proto/system.proto) | `SystemSample`、`Cpu`、`CpuCore`、`Memory`，以及保留的旧 `GpuSample` 定义。 |
| [process.proto](proto/process.proto) | `ProcessSample`、`Process`、`Thread`。 |

系统和进程协议分别导入公共协议。三个文件共用 `syslens.v1` package，生成的 Rust 类型仍位于 `protocol::pb`；消息名称、字段编号和编码格式保持兼容。[build.rs](build.rs) 编译这些文件，并将全部协议及 Timestamp 依赖打包到同一份 descriptor，供 MCAP 和 Foxglove 使用。

每个采集器在自己的线程按配置周期运行，完成后更新最新结果；组装器按发布周期读取这些结果，不等待任何采集器。采集期间不持有共享快照锁。同一份 GPU 内存结果可供系统和进程组装器复用，框架只接收通用 `Schedule`，不了解指标类型。

调度采用单调时钟，超时会跳过错过的时间点，继续沿原时间网格运行，不连续补采，也不在慢采集结束后额外等待完整周期。发布时形成一致的缓存快照；`header.timestamp` 是快照/发布时间，不代表各指标同时采集。

`header.inputs` 提供每个输入的诊断信息：

| 字段 | 含义 |
| --- | --- |
| `timestamp` | 该输入最近一次采集开始的 Unix 时间，尚未完成首采时缺失。 |
| `interval_ms` / `actual_interval_ms` | 配置周期 / 最近两次采集开始的实际间隔；首采无实际间隔。 |
| `collection_duration_ms` | 最近一次采集调用的耗时。 |
| `age_ms` / `stale` | 基于单调时钟计算的数据年龄 / 是否超过过期阈值。 |
| `sample_sequence` | 该输入的观测序号；相同序号表示多个消息复用了同一次观测。 |
| `missed_periods` | 采集线程累计跳过的调度时间点数量。 |
| `skipped_samples` | 已完成但在发布前被更新观测覆盖的累计样本数。 |
| `error` | 尚未就绪、采集错误或过期原因。 |

尚未完成、失败或过期的输入不填入指标；未过期的慢速输入可被多条消息复用，并保留真实采集时间。进程 CPU、内存、线程、GPU 数据按 `(PID, start_ticks)` 关联。GPU 的 `utilization_timestamp` / `memory_timestamp` 另外保存来源的实际读取/接收时间，驱动内部的统计窗口仍由接口决定。

缓存有界，每个采集器只保存最新结果。采样快于发布、线程调度抖动或输出背压均可能覆盖中间观测，`skipped_samples` 明确记录这种情况；MCAP 保存的是每条已组装消息，不能恢复尚未发布的被覆盖观测。需要更细时间分辨率时应同时缩短发布周期，并检查实际间隔、丢失计数和本程序开销。程序错误（例如重复注册、依赖未注册、相同 Topic 冲突 Schema）仍会报错。

## 录制与输出行为

- 录制队列满时对消息发布施加背压；采集线程继续更新缓存，已组装并进入输出流程的消息不会因队列满而丢弃；磁盘写入错误会使程序报错退出。
- 实时队列满时丢弃新到达的实时消息，并在退出时报告数量。实时发送为尽力而为，慢客户端还受 SDK 内部队列约束。
- 两种输出共享 Protobuf 编码后的字节；各有工作线程和队列，互不调用。
- MCAP 使用 Zstd 压缩，默认每 300 秒或累计 64 MiB 未压缩消息负载后，在下一条消息前轮转；后者不是精确文件大小上限。
- `recording.flush_ms` 只控制刷盘，不控制采集；持续写入时默认每 1000 ms 完成并 flush 当前 chunk，分段结束和正常退出时写入索引、footer 并同步文件。强制终止或掉电不能保证最后一段完整。
- 队列容量按消息条数限制；大量进程/线程会使单条进程消息变大，可用 PID/名称筛选降低开销。

## 扩展采集能力

新增指标时：

1. 在 `data` 定义普通 Rust 数据，在 `collectors` 实现 `Collector<Data = YourData>`。
2. 在 `app` 创建 `Key<YourData>`，用 `Runtime::add_collector(key, collector, schedule)` 注册，并在配置中提供该指标的周期；框架内部不需要增加指标枚举或分支。
3. 让现有组装器增加该输入，或新增组装器；通过 `dependencies()` 声明 Key 名称，再通过 `batch.get(key)` 读取当前缓存快照中的有效结果。
4. 按职责扩展 `proto/common.proto`、`proto/system.proto` 或 `proto/process.proto`，由组装器构造消息并调用 `protocol::encode`。新增协议文件时同步更新 `build.rs` 的编译列表。同一个采集结果可以供多个组装器使用。
5. 在 `app` 注册组装器。两个输出模块无需修改。

Protobuf 已发布字段的编号不能重用，删除字段时保留编号和名称；新增指标优先增加字段或消息。接口与完整消息定义分别见 [framework](src/framework) 和 [proto](proto)。

## 验证

在 Linux 执行：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试覆盖计数器差分、核上下线、PID 复用、Jetson 输出解析与启动探测（失败不再重试、超时、子进程复用）、独立周期与慢源隔离、过期/失败输入、调度超时、有界队列与关闭、MCAP 分段及 Schema、真实 Linux 进程/线程到 MCAP，以及真实 WebSocket 订阅后的 Protobuf 和时间戳验证。

开发验证环境为 ARM64 Linux 容器。GPU 测试使用 sysfs、tegrastats、nvmap 与 nvidia-smi 格式样本及模拟命令；覆盖 N/A、来源停用、超时和进程身份匹配。尚未在实体 Orin/Thor 上验证，驱动/BSP 的接口可用性及权限仍需在目标机器确认。
