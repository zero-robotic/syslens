#!/usr/bin/env python3
"""Generate the importable Foxglove layout using only Python's standard library."""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
SYSTEM = "/syslens/system"
PROCESSES = "/syslens/processes"
CURVES = "/foxglove_script/syslens/process_curves"
COLORS = ["#249EFF", "#00B894", "#FFAD33", "#A879FF", "#FF6384"]
panels = {}


def series(path, label, *, dynamic=None):
    result = {
        "value": path,
        "enabled": True,
        "timestampMethod": "receiveTime",
        "showLine": True,
        "lineSize": 1.5,
        "label": label,
    }
    if dynamic:
        result.update(dynamicLabelField=dynamic[0], dynamicValueField=dynamic[1])
    return result


def plot(key, title, paths, unit, maximum=None):
    panel_id = f"Plot!syslens-{key}"
    for index, path in enumerate(paths):
        path["id"] = f"{key}-{index}"
        if "dynamicLabelField" not in path:
            path["color"] = COLORS[index % len(COLORS)]
    config = {
        "foxglovePanelTitle": title,
        "paths": paths,
        "xAxisVal": "timestamp",
        "timeWindowMode": "sliding",
        "followingViewWidth": 60,
        "playbackBarPosition": "right",
        "isSynced": True,
        # Each Plot keeps its native toolbar/settings toggle for showing the legend.
        "showLegend": False,
        "showPlotValuesInLegend": True,
        "legendDisplay": "left",
        "sidebarDimension": 220,
        "showXAxisLabels": True,
        "showYAxisLabels": True,
        "showYAxisGridLines": True,
        "yAxisLabel": unit,
        "minYValue": 0,
    }
    if maximum is not None:
        config["maxYValue"] = maximum
    panels[panel_id] = config
    return panel_id


def split(first, second, direction="row", percentage=50):
    return {"direction": direction, "first": first, "second": second, "splitPercentage": percentage}


def grid(items):
    if len(items) == 1:
        return items[0]
    if len(items) == 2:
        return split(*items)
    assert len(items) in (4, 6)
    return split(split(*items[:2]), grid(items[2:]), "column", 200 / len(items))


def memory_series(field, label):
    return series(f"{SYSTEM}.memory.{field}.@div(1048576)", label)


def process_plot(key, title, field, unit, maximum=None):
    return plot(key, title, [series(f"{CURVES}.{field}[:]", title, dynamic=("label", "value"))], unit, maximum)


tabs = []


def tab(title, items):
    tabs.append({"title": title, "layout": grid(items)})


tab("系统统计数据", [
    plot("cpu", "系统 CPU 利用率", [series(f"{SYSTEM}.cpu.usage_percent", "总 CPU")], "%", 100),
    plot("cores", "各逻辑核 CPU 利用率", [series(f"{SYSTEM}.cpu.cores[:]{'{usage_percent>=0}'}", "逻辑核", dynamic=("core_id", "usage_percent"))], "%", 100),
    plot("memory", "系统内存", [memory_series("total_bytes", "总量"), memory_series("used_bytes", "已用"), memory_series("available_bytes", "可用")], "MiB"),
    plot("swap", "系统 Swap", [memory_series("swap_total_bytes", "总量"), memory_series("swap_used_bytes", "已用")], "MiB"),
    plot("gpu", "系统 GPU 利用率", [series(f"{SYSTEM}.gpu.utilization_percent", "GPU")], "%", 100),
    plot("gpu-memory", "系统 GPU 内存（驱动统计）", [series(f"{SYSTEM}.gpu.memory_used_bytes.@div(1048576)", "已用"), series(f"{SYSTEM}.gpu.memory_total_bytes.@div(1048576)", "总量（接口支持时）")], "MiB"),
])
tab("进程统计数据", [
    process_plot("process-cpu", "各进程 CPU（单核满载为 100%）", "cpu", "%"),
    process_plot("process-rss", "各进程 RSS", "rss_mib", "MiB"),
    process_plot("process-virtual", "各进程虚拟内存", "virtual_mib", "MiB"),
    plot("skipped", "进程扫描跳过数", [series(f"{PROCESSES}.skipped_processes", "跳过进程（CPU 来源）")], "个"),
    process_plot("process-gpu", "各进程 GPU 利用率", "gpu_utilization", "%", 100),
    process_plot("process-gpu-memory", "各进程 GPU 内存（驱动统计）", "gpu_memory_mib", "MiB"),
])
tab("线程 CPU", [process_plot("threads", "各线程 CPU（pid / tid 可筛选）", "thread_cpu", "%")])
diagnostics = []
for field, title, unit in (
    ("actual_interval_ms", "实际采集间隔", "ms"),
    ("collection_duration_ms", "单次采集耗时", "ms"),
    ("age_ms", "距采集完成时长（ms）", "ms"),
    ("missed_periods", "累计错过的调度周期", "次"),
):
    diagnostics.append(plot(field, title, [
        series(f"{topic}.header.inputs[:]{'{'}{field}>=0{'}'}", label, dynamic=("collector", field))
        for topic, label in ((SYSTEM, "系统消息输入"), (PROCESSES, "进程消息输入"))
    ], unit))
tab("采集状态", diagnostics)

for key, topic, title in (("system", SYSTEM, "系统原始消息 / GPU 来源与缺失原因"), ("processes", PROCESSES, "进程原始消息 / 实例 ID 与线程错误")):
    panels[f"RawMessages!syslens-{key}"] = {"topicPath": topic, "foxglovePanelTitle": title}
tab("原始消息", ["RawMessages!syslens-system", "RawMessages!syslens-processes"])
panels["Markdown!syslens-help"] = {
    "foxglovePanelTitle": "Syslens 使用说明",
    "markdown": "# Syslens 监控曲线\n\n连接采集主机的 Foxglove WebSocket（默认端口 8765），或打开 Syslens 的 MCAP 文件。布局不会自动连接主机。\n\n- 右侧 Variables / 变量（按 ] 打开）：`pid = 0` 显示全部进程，改成目标 PID 可筛选所有进程和线程曲线。`tid = 0` 显示全部线程，非零时筛选线程。数值请保持为整数。\n- 所有图表默认隐藏图例列表。点击各图工具栏的 Toggle legend / 切换图例，或在齿轮设置的 Legend → Show legend 中单独开启或关闭。\n- 每条进程 / 线程曲线按完整 instance_id 区分，名称相同或 PID 复用不会混线。\n- 每个动态序列最多显示 100 个实体；大量进程 / 线程时先设置 pid，或在采集端按 PID / 名称筛选。\n- 内存单位为 MiB（1024² 字节）；进程 CPU 可超过 100%。\n- GPU 不支持、CPU 缺少差分基线、输入失败或过期时会缺少曲线；不会将缺失值补成 0。原始消息页可查看 header.inputs 和 GPU error / memory_scope。\n- 默认显示最近 60 秒。回放全程时将 Plot 的 Time window 改为 Automatic。\n- 实时显示会复用尚未过期的指标，且 Foxglove 输出会按 Topic 合并消息；精确分析单一来源的原始观测请使用 MCAP 并按 header.trigger_input 筛选。\n\n需要支持 Plot 动态子序列和布局内 User Scripts 的 Foxglove 客户端；开发核对版本为 3.3.0。\n",
}
tab("使用说明", ["Markdown!syslens-help"])
panels["Markdown!syslens-help"]["markdown"] += """
## 进程过滤条件

右侧 Variables / 变量中的条件可同时使用：

| 变量 | 默认值 | 作用 |
| --- | --- | --- |
| `command_contains` | 空字符串 | 命令行包含指定文本，区分大小写；如 `python3` 或 `--mode test`。与 pid 共同筛选进程及其线程。 |
| `cpu_min_percent` | 0 | 进程 CPU 下限，单位 %；例如 5 表示 ≥ 5%。 |
| `rss_min_mib` | 0 | 进程 RSS 下限，单位 MiB；例如 100 表示 ≥ 100 MiB。 |
| `virtual_min_mib` | 0 | 进程虚拟内存下限，单位 MiB。 |
| `gpu_min_percent` | 0 | 进程 GPU 利用率下限，单位 %。 |
| `gpu_memory_min_mib` | 0 | 进程 GPU 内存下限，单位 MiB。 |
| `thread_cpu_min_percent` | 0 | 线程 CPU 下限，单位 %。 |

每个下限只影响对应图表，按每条消息的当前值判断，低于下限的点不输出；不是按历史峰值选中整条曲线。下限为 0 时保留有效零值，负数或非有限下限按 0 处理。扫描跳过数是整体统计，不参与筛选。

筛选仅影响客户端曲线，MCAP 保留全部采集结果。命令行来自 /proc/PID/cmdline，随进程内存一起刷新（默认 200 ms）。空关键词关闭命令行筛选；启用时，空命令行、读取失败及旧录制中没有该字段的进程不匹配。原始消息中的 command_line_error 可查看读取失败原因。
"""
panels["Tab!syslens"] = {"activeTabIdx": 0, "tabs": tabs}
layout = {
    "configById": panels,
    "globalVariables": {
        "pid": 0, "tid": 0, "command_contains": "",
        "cpu_min_percent": 0, "rss_min_mib": 0, "virtual_min_mib": 0,
        "gpu_min_percent": 0, "gpu_memory_min_mib": 0, "thread_cpu_min_percent": 0,
    },
    "userNodes": {
        "syslens-process-curves": {
            "name": "Syslens process curves",
            "sourceCode": (HERE / "process_curves.ts").read_text(encoding="utf-8"),
        }
    },
    "linkedGlobalVariables": [],
    "playbackConfig": {"speed": 1, "messageOrder": "receiveTime"},
    "layout": "Tab!syslens",
}
(HERE / "syslens.json").write_text(json.dumps(layout, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(f"Generated {len(tabs)} tabs and {sum(key.startswith('Plot!') for key in panels)} plots in {HERE / 'syslens.json'}")
