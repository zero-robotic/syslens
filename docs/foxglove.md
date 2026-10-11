# Foxglove 曲线布局

导入 [config/foxglove/syslens.json](../config/foxglove/syslens.json) 即可使用预设曲线。布局包含 6 个标签页、17 个 Plot 面板、两个原始消息面板和使用说明；数据来自 `/syslens/system` 与 `/syslens/processes`。

## 连接与导入

1. 在采集主机启动 Syslens。供其他主机访问时执行 `./target/release/syslens --bind 0.0.0.0`；仅本机访问时保留默认监听地址即可。
2. 在 Foxglove 选择 **Open connection → Foxglove WebSocket**，输入 `ws://采集主机IP:8765`。本机连接使用 `ws://127.0.0.1:8765`。
3. 从 **Layouts → Import from file…** 导入 `config/foxglove/syslens.json`。各面板根据表达式订阅所需 Topic；无需逐项添加曲线。布局文件不保存或自动建立主机连接。[官方布局导入说明](https://docs.foxglove.dev/docs/visualization/layouts)
4. 回放时直接打开 Syslens 录制的 `.mcap` 文件，沿用相同布局，不必启动采集程序。

需要支持 Plot 动态子序列及布局内 User Scripts 的 Foxglove 客户端。本布局已在 Foxglove 3.3.0 导入并用本地模拟 MCAP 验证全部曲线页及 PID 筛选；实际 Jetson 实时数据仍需在目标主机连接验证。默认各图同步时间轴，显示最近 60 秒；查看完整回放时，可将 Plot 的 **Time window** 改为 **Automatic**。

## 页面与指标

| 标签页 | 曲线 |
| --- | --- |
| 系统统计数据 | 系统 CPU、每个逻辑核 CPU；内存总量 / 已用 / 可用；Swap 总量 / 已用；GPU 利用率、驱动统计的 GPU 内存已用 / 总量。 |
| 进程统计数据 | 各进程 CPU、RSS、虚拟内存；进程 CPU 扫描的跳过数；各进程 GPU 利用率与 GPU 内存占用。 |
| 线程 CPU | 各线程 CPU，区分所属进程和线程实例。 |
| 采集状态 | 各输入实际采集间隔、采集耗时、输入年龄、累计错过周期。 |
| 原始消息 | 系统 / 进程原始消息，用于查看来源、错误、instance_id 和 memory_scope。 |
| 使用说明 | 连接、筛选、单位和缺失值说明。 |

“系统统计数据”采用两列三行布局，依次展示 CPU、内存 / Swap、GPU 的六个图表。

“进程统计数据”同样采用两列三行布局，依次展示 CPU / RSS、虚拟内存 / 扫描跳过数、GPU 利用率 / GPU 内存的六个图表，支持通过 `pid` 筛选进程曲线。

内存使用 MiB（1 MiB = 1,048,576 字节）。进程 CPU 占满一个逻辑核为 100%，多线程可超过 100%，对应图的纵轴不固定为 100%。GPU 内存沿用驱动统计口径，不能与系统 RAM 或 RSS 直接相加。

## 图例显示开关

每个图表的曲线名称与数值列表称为**图例（Legend）**。预设布局的 17 个 Plot 面板默认隐藏图例，让曲线区域使用完整宽度；隐藏图例不会关闭曲线或停止订阅。

需要查看列表时，可点击该图表工具栏的 **Toggle legend / 切换图例** 按钮，也可打开齿轮设置，在 **Legend → Show legend** 中开启或关闭。开关对每个面板独立生效；开启后仍使用左侧图例并显示数值。若希望图例显示在图内，可将 **Position** 改为 **Floating**。[官方图例设置与工具栏说明](https://docs.foxglove.dev/docs/visualization/panels/plot#legend)

已经导入的布局不会随仓库文件更新而自动改变。可以直接在现有图表中关闭图例，或重新导入更新后的 `config/foxglove/syslens.json`；希望保留面板调整时请保存布局。

## 进程与线程筛选

在 Foxglove 右侧 **Variables / 变量** 中修改数值（按 `]` 打开右侧栏）：

| 变量 | 默认值 | 行为 |
| --- | ---: | --- |
| `pid` | 0 | 显示全部进程；设为具体 PID 后筛选进程 CPU、内存、GPU 和线程曲线。 |
| `tid` | 0 | 显示全部线程；设为具体 TID 后筛选线程 CPU。建议同时指定所属 PID。 |

筛选只影响客户端曲线，不改变采集范围和 MCAP 内容。若要减少采集开销，在 Syslens 配置中设置 `processes.pids` / `processes.names` 或使用 `--pid`。

逻辑核以 `core_id` 分线；进程和线程以包含 boot ID 与启动 tick 的完整 `instance_id` 分线，并在图例中附带名称，避免数组重排、同名进程、PID / TID 复用导致混线。每个动态父序列最多显示 100 个实体，超过后应按 PID / TID 缩小范围。[官方动态曲线说明](https://docs.foxglove.dev/docs/visualization/panels/plot#dynamic-series)

## 客户端数据转换与时间语义

布局内嵌一个 User Script，将进程消息转换为带 `label` / `value` 的数组，并展开嵌套的 GPU、线程指标。派生 Topic 为 `/foxglove_script/syslens/process_curves`，只存在于 Foxglove 客户端；采集程序仍发送原来的两个 Topic，MCAP 协议不变。该脚本随布局导入，无需安装扩展或另行加载 TypeScript 文件。[官方 User Scripts 说明](https://docs.foxglove.dev/docs/visualization/user-scripts)

所有曲线使用 `receiveTime`（界面中的 Log time）。Syslens 的 Foxglove 输出向 SDK 提供采集开始时间作为消息 log time，MCAP 也使用相同时间戳。没有使用 ROS 专用的 `header.stamp`；当前消息字段为 `header.timestamp`。

默认展示每条消息中仍有效的组合值，不筛选 `header.trigger_input`，以适配实时输出按 Topic 合并消息的行为。部分点可能是其他来源的复用值。需要逐次分析某个采集器时，打开 MCAP 并按触发来源过滤，例如 `/syslens/system{header.trigger_input=="cpu"}.cpu.usage_percent`。

没有基线、输入过期、采集关闭或驱动不支持时，相应指标会缺失；转换脚本保留有效的 0，跳过缺失和非有限数值。GPU 图为空时先查看原始消息中的 `header.inputs`、`gpu.utilization_error`、`gpu.memory_error`，不要将空曲线解释为零负载。首次只有系统 Topic 或禁用进程采集时，进程脚本及相关面板可能提示缺少输入，这是配置与数据源不匹配的提示。

## 修改与重新生成

布局生成入口为 [generate_layout.py](../config/foxglove/generate_layout.py)，进程转换源码为 [process_curves.ts](../config/foxglove/process_curves.ts)。修改后执行：

```sh
python3 config/foxglove/generate_layout.py
```

脚本只依赖 Python 标准库，生成完整的 `syslens.json`。导入时只需要该 JSON 文件；直接在 Foxglove 调整并导出的布局，也可以单独使用。
