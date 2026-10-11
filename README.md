# Syslens

Rust 编写的 Linux 进程与系统监控程序，面向 NVIDIA Jetson Orin、Thor 集成 GPU。采集结果使用 Protobuf 编码，通过 Foxglove Rust SDK 实时发送，并独立写入 MCAP 文件。

一个 Cargo package、一个二进制 crate，通过模块实现分层。

## 构建与运行

需要 Linux、Rust 1.89 或更新版本及 C/C++ 构建工具。Protobuf 编译器由构建依赖提供，无需另外安装 protoc。在目标 Jetson 上构建可避免不同 JetPack 系统的运行库兼容问题。

```sh
cargo build --release --locked

# 默认加载 config/syslens.toml，按各指标独立周期采集，实时曲线每个 Topic 最多每 100 ms 更新一次。
./target/release/syslens

# 监控指定进程，允许其他主机连接实时数据。
./target/release/syslens --pid 1234,5678 --bind 0.0.0.0

# 普通 Linux 主机：禁用 Jetson GPU，录制约 6 秒，保存期间所有已组装观测。
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
| `process_cpu_ms` | 100 | 进程 CPU；其他进程指标独立发现实体。 |
| `process_memory_ms` | 200 | 进程 RSS、虚拟内存及该来源观察到的进程身份。 |
| `thread_cpu_ms` | 100 | 进程内各线程 CPU。 |
| `gpu_utilization_ms` | 100 | 系统 GPU 利用率；也控制 tegrastats 的输出间隔。 |
| `gpu_memory_ms` | 500 | 系统及进程 GPU 内存，共享驱动内存快照。 |
| `process_gpu_utilization_ms` | 1000 | 进程 GPU 利用率；pmon 限制至少 1000 ms。 |
| `stale_after_periods` | 3 | 输入超过自身采集周期的此倍数后过期，组装时不使用。 |

顶层 `interval_ms = 100` 现在是 **Foxglove 每个 Topic 的实时发送间隔**，不限制组装或录制频率。`--interval-ms` 只覆盖该值。`--samples N` 保留为有界运行选项，运行时长为 `N × interval_ms` 毫秒，随后排空队列；它不再表示消息条数。首个观测完成即可组装，不等待其他来源就绪。

例如，`process_cpu_ms = 20`、`thread_cpu_ms = 20`、`interval_ms = 100` 会将每次进程/线程观测写入 MCAP，实时端每个 Topic 最多每 100 ms 发送最新消息。缩短采集周期不需要同步缩短实时发送间隔。高频扫描全部进程/线程仍有开销，应按 PID/名称限定目标，并在目标设备测量采集耗时。

`[runtime]` 配置观测队列容量、入队等待上限、单次采集超时、启动探测预算及退出预算；启动探测也使用受监督任务，超出总预算则启动失败。`[recording]`、`[foxglove]` 各自配置队列容量、`overflow`、`on_failure` 和单次输出超时，默认值及说明见配置文件。

PID 和进程名选择器按“或”匹配；名称精确匹配 `/proc/PID/stat` 的 `comm`。两个选择器都为空时监控所有可见进程。`--no-threads` 可关闭线程扫描；配置中 `processes.enabled = false` 可关闭进程及线程采集。

默认监听 `127.0.0.1:8765`，录制写入 `recordings/`。按 Ctrl+C 或发送 SIGTERM 后，程序取消采集，继续处理已完成的观测，再排空输出队列并结束 MCAP 文件；退出受 `[runtime].shutdown_timeout_ms` 约束。录制与实时发送可分别关闭，但至少保留一个输出。

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

仍然只有两种消息：`SystemSample`（`/syslens/system`）和 `ProcessSample`（`/syslens/processes`）。每个观测只触发依赖它的组装器：例如 CPU 观测触发系统消息，线程观测触发进程消息，共享 GPU 内存观测可触发两者。消息数不再固定为每周期两条。实时输出按 Topic 合并待发消息，录制保留全部已组装消息。

`header.trigger_input` 标明触发来源，`header.timestamp` 及 MCAP/Foxglove 时间戳使用该观测的采集开始时间。其他指标可复用未过期的输入，实际时间见 `header.inputs` 和 GPU 时间字段；回放分析某个输入的原始时间序列时按 `trigger_input` 筛选，避免把复用值当作新观测。不同来源并发采集，消息的采集时间不保证严格单调。

关闭进程采集后只有系统消息。旧 `GpuSample` 定义保留为 deprecated，不再发布；旧 MCAP 使用文件内的 schema 回放。

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

GPU load 节点随平台与 JetPack 版本变化，可用 `gpu.load_path` 显式指定。节点不存在、不可读或无有效数据时继续尝试其他来源；需让 `tegrastats` 在 PATH 内或配置其完整路径。解析支持单频率和 Orin/Thor 多 GPC 数组；只有频率而没有百分比时不生成利用率。[tegrastats 格式说明](https://docs.nvidia.com/jetson/archives/r38.4/DeveloperGuide/AT/JetsonLinuxDevelopmentTools/TegrastatsUtility.html)

GPU 来源选择集中在 `collectors::gpu_sources`；启动时分别探测系统利用率、系统内存、进程内存和进程利用率接口。系统查询失败不阻止进程接口探测。失败的来源只提示一次，本次运行不再尝试；部分来源失败不影响其他来源。显式报告所有活动进程值均为 N/A 的接口也会停用；空进程表本身不代表失败，允许后续新进程被采集。修复驱动、路径或权限后需要重启。`--no-gpu` 或 `gpu.enabled = false` 直接跳过全部 GPU 探测，消息中的 `gpu` 缺失；启用但不可用时将带类型的失败输入注册到框架，`header.inputs` 和指标的 `utilization_error` / `memory_error` 均可看到原因。

探测数据不发送，运行时读取新观测。tegrastats 按 `sampling.gpu_utilization_ms` 持续输出；采集器通过非阻塞管道按顺序消费输出，不使用无法取消的读取线程。探测启动的子进程继续使用。tegrastats 和 nvidia-smi 共用可取消、有截止时间的子进程封装；nvidia-smi 使用只读命令，每次最多接收 4 MiB 输出，默认超时为 `gpu.timeout_ms = 1500`；使用较慢的 tegrastats 输出周期时需相应调整超时。

系统 GPU 利用率、GPU 内存、进程 GPU 利用率分别在独立采集线程运行，pmon 不再阻塞内存或 CPU 采集。GPU 内存采集内部的系统/进程查询仍共用一次采集调用，驱动内存和进程利用率使用不同 Rust 数据类型。普通读取失败允许下次重试，框架监督到任务超时、崩溃或入队超时则停用该来源并报告；失败观测不会继续使用旧成功值。

GPU 指标含独立的来源、实际读取时间和缺失原因。进程 GPU 数据在查询前后核对 `/proc/PID/stat` 的启动 tick，组装时再按 `(PID, start_ticks)` 匹配，避免 PID 复用混淆。退出或命令失败时只终止本程序创建的子进程组，避免其后代持有 stdout 导致关闭卡住。

CPU 采集器的首次观测、首次出现的进程/线程/逻辑核以及计数器重置后没有有效差分，利用率字段保持缺失。每个来源第一次观测即可发送，因此消息允许部分指标尚未就绪。进程内存尚未采集、过期或身份不匹配时，`rss_bytes` / `virtual_bytes` 缺失；`cpu_time_ticks` 也使用 optional presence，CPU 不可用时不伪造 0；字段编号和 wire type 保持兼容。进程组装器按各个有效输入的 `(PID, start_ticks)` 并集合并实体，不以 CPU 输入作为进程名单。CPU 不可用时仍输出内存、线程或 GPU 信息；不同启动 tick 的同一 PID 保留为不同实例，旧来源过期后移除。进程与线程标识包含系统 boot ID、PID/TID 和启动 tick；退出、PID 复用、核上下线不会沿用旧实体的基线。无法读取的进程会跳过并计数，线程不完整情况写入 `threads_error`。

## 分层与数据流

```text
Collectors (独立周期、普通 Rust 数据)
    |
    v
有界观测 FIFO (保留每次完成的观测；入队等待有上限)
    |
    v
Runtime (类型注册、依赖校验、更新输入视图、按观测触发)
    |
    v
Assemblers (合并领域数据 -> Protobuf 编码一次)
    |
    v
Dispatcher (非阻塞分发同一份字节)
    +--> 录制 FIFO ----------------------> MCAP
    +--> 按 Topic 保留最新待发值、独立限频 -> Foxglove

OperationContext / Task 统一管理取消、截止时间和退出监督
```

| 模块 | 职责与依赖边界 |
| --- | --- |
| `app` / `config` | 配置、信号处理、创建并注册组件；不承担驱动来源选择。 |
| `framework` | 通用 Collector、Assembler、Output 接口；有界观测流、类型化依赖、调度与任务监督、输出隔离。不了解具体指标与 Protobuf 类型。 |
| `data` | 采集器输出的普通 Rust 数据结构，独立于协议。 |
| `collectors` | 读取与计算指标、维护差分基线；不依赖 Protobuf、组装器或输出实现。 |
| `assemblers` | 声明带类型的依赖，读取 Batch，按领域身份组合输入并编码；不调用采集器、探测来源或选择输出策略。 |
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

每个采集器在自己的线程按单调时钟调度。已完成的观测先进入 FIFO，Runtime 逐个更新输入视图并触发所有相关组装器。慢来源不阻塞其他来源。调度错过时间点时跳过，不补发虚构观测。

类型依赖包含名称和 `TypeId`，注册时校验。`batch.read(key)` 返回 `Result<Option<&T>>`：`None` 表示尚未就绪、失败或过期；未声明的依赖和类型错误返回 `Err`，组装器必须传播。所有过期判断使用单调时间；触发观测必须被保留，其他输入按触发观测完成时的年龄判断。排队或磁盘延迟不会把历史触发观测过滤掉。

`header.inputs` 提供诊断信息：

| 字段 | 含义 |
| --- | --- |
| `timestamp` | 该输入观测开始的 Unix 时间；尚未完成或永久不可用时缺失。 |
| `interval_ms` / `actual_interval_ms` | 配置周期 / 最近两次采集开始的实际间隔。 |
| `collection_duration_ms` | 最近一次采集调用的耗时。 |
| `age_ms` / `stale` | 从该输入采集完成到触发观测完成的单调时间差 / 是否过期。 |
| `sample_sequence` | 来源观测序号；复用值保留原序号。 |
| `missed_periods` | 采集线程累计跳过的调度时间点数量。 |
| `skipped_samples` | 旧缓存覆盖计数，已 deprecated；新观测流始终填 0，不能作为输出丢失计数。 |
| `error` | 尚未就绪、不可用、读取错误、过期或任务停用原因。 |

## 录制、过载与退出

- MCAP 默认使用完整 FIFO。队列满时默认 `overflow = "disable"`：立即报告并停止接受后续录制消息，排空已接收数据，实时输出继续工作。磁盘持续跟不上时不承诺无限期无损。
- Foxglove 默认按 Topic 合并待发值，单独按 `interval_ms` 限频；这是显示降采样，不影响录制。SDK 内部的慢客户端队列仍属于尽力发送。
- 每个输出支持 `overflow = "disable" / "drop_newest" / "drop_oldest"` 和 `on_failure = "isolate" / "stop"`。默认隔离失败；选择 `stop` 可要求该输出故障时结束整个运行。所有输出或所有采集任务失效也会结束运行。丢弃和停用立即写 stderr，退出时汇总；发生输出故障的运行最终返回错误。
- 观测 FIFO 满时只暂停生产者的后续采集，不覆盖其已完成观测。超过 `runtime.enqueue_timeout_ms` 仍不能入队，则报告未交付序号并停用该来源。队列容量按条数限制，不是字节数；大进程/线程集合应配合筛选与容量规划。
- MCAP 使用 Zstd 压缩，默认每 300 秒或累计 64 MiB 未压缩负载后轮转。`flush_ms` 控制 chunk flush；分段结束和正常退出时写索引、footer 并同步文件。强制终止、掉电或 I/O 超时不能保证当前分段完整。
- 采集和输出共用 `OperationContext` 的取消/截止时间约定。子进程使用非阻塞读取，进程/线程扫描定期检查取消；Foxglove 关闭也遵守截止时间。
- 退出预算默认 5000 ms。先取消采集并处理已完成观测，最多使用一半预算；剩余预算用于所有输出并行排空与收尾。Rust 不能强制终止不合作的线程或内核中卡住的 I/O，因此超时会明确返回错误并分离该线程，不无限等待，也不声称数据已完整落盘。

## 扩展采集能力

新增指标时：

1. 在 `data` 定义普通 Rust 数据，在 `collectors` 实现 `Collector<Data = YourData>`，在长循环和 I/O 等待中遵守 `OperationContext`。
2. 在 `app` 创建 `Key<YourData>`，用 `Runtime::add_collector(key, collector, schedule)` 注册，并在配置中提供该指标的周期；框架内部不需要增加指标枚举或分支。
3. 让现有组装器增加该输入，或新增组装器；通过 `key.dependency()` 声明带类型的依赖，再通过 `batch.read(key)?` 读取有效结果。依赖不可用时处理 `None`，不要吞掉接线错误。
4. 按职责扩展 `proto/common.proto`、`proto/system.proto` 或 `proto/process.proto`，由组装器构造消息并调用 `protocol::encode`。新增协议文件时同步更新 `build.rs` 的编译列表。同一个采集结果可以供多个组装器使用。
5. 在 `app` 注册组装器；可选能力探测失败时使用 `add_capability` / `add_unavailable` 注册原因。两个输出模块无需修改。

Protobuf 已发布字段的编号不能重用，删除字段时保留编号和名称；新增指标优先增加字段或消息。接口与完整消息定义分别见 [framework](src/framework) 和 [proto](proto)。

## 验证

在 Linux 执行：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试覆盖高频观测逐条保留、实时降采样、慢输出与失败隔离、显式停止策略、类型依赖校验、进程多来源合并与 PID 复用、GPU 分项探测与进程身份核对、可取消子进程及退出时间上限；同时验证真实 Linux 进程/线程进入完整 MCAP、分段 Schema，以及真实 WebSocket 订阅后的 Protobuf 和时间戳。

开发验证环境为 ARM64 Linux 容器。GPU 测试使用 sysfs、tegrastats、nvmap 与 nvidia-smi 格式样本及模拟命令；覆盖 N/A、来源停用、超时和进程身份匹配。尚未在实体 Orin/Thor 上验证，驱动/BSP 的接口可用性及权限仍需在目标机器确认。
