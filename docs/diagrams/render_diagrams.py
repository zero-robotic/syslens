#!/usr/bin/env python3
"""Draw the repository architecture diagrams as SVG and render portable PNGs.

Requires Python 3, rsvg-convert, and a Chinese font. Run from any directory.
"""

from html import escape
from pathlib import Path
import os
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
INK = "#19334B"
MUTED = "#526A7D"
BLUE = "#2864B4"
TEAL = "#137D75"
PURPLE = "#7653AD"
AMBER = "#9A671C"
FONT = "Hiragino Sans GB, Noto Sans CJK SC, Microsoft YaHei, sans-serif"


class Drawing:
    def __init__(self, height, number, title, subtitle):
        self.height = height
        self.parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="1440" height="{height}" viewBox="0 0 1440 {height}" role="img">',
            f"<title>{escape(title)}</title><desc>{escape(subtitle)}</desc>",
            '<defs><marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="context-stroke"/></marker></defs>',
            '<rect width="1440" height="100%" fill="#F5F8FC"/>',
        ]
        self.rect(48, 34, 58, 44, BLUE, radius=12)
        self.text(77, 65, number, 24, "#FFFFFF", anchor="middle", bold=True)
        self.text(126, 68, title, 32, bold=True)
        self.text(50, 108, subtitle, 18, MUTED)

    def rect(self, x, y, w, h, fill="#FFFFFF", stroke=None, radius=16):
        self.parts.append(
            f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{radius}" fill="{fill}" '
            f'stroke="{stroke or fill}" stroke-width="1.5"/>'
        )

    def text(self, x, y, value, size=20, color=INK, anchor="start", bold=False, leading=None):
        lines = value.split("\n")
        line_height = leading or round(size * 1.5)
        spans = "".join(
            f'<tspan x="{x}" dy="{0 if i == 0 else line_height}">{escape(line)}</tspan>'
            for i, line in enumerate(lines)
        )
        self.parts.append(
            f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
            f'fill="{color}" text-anchor="{anchor}" font-weight="{600 if bold else 400}">{spans}</text>'
        )

    def card(self, x, y, w, h, title, body, accent=BLUE, fill="#FFFFFF", size=20):
        self.rect(x, y, w, h, fill, "#CFDAE5")
        compact = h <= 100
        self.rect(x + 16, y + (12 if compact else 22), 5, 29, accent, radius=2)
        self.text(x + 32, y + (34 if compact else 44), title, 24, accent, bold=True)
        self.text(x + 32, y + (65 if compact else 80), body, size, leading=30)

    def arrow(self, points, color=MUTED, dashed=False, end=True):
        coords = " ".join(f"{x},{y}" for x, y in points)
        dash = 'stroke-dasharray="7 6"' if dashed else ""
        marker = 'marker-end="url(#arrow)"' if end else ""
        self.parts.append(
            f'<polyline points="{coords}" fill="none" stroke="{color}" stroke-width="2.5" '
            f'stroke-linejoin="round" {dash} {marker}/>'
        )

    def footer(self, value):
        self.text(50, self.height - 28, value, 17, MUTED)

    def save(self, name, renderer, env):
        directory = ROOT / "images"
        directory.mkdir(parents=True, exist_ok=True)
        svg = directory / f"{name}.svg"
        svg.write_text("\n".join(self.parts + ["</svg>"]) + "\n", encoding="utf-8")
        subprocess.run([renderer, str(svg), "-o", str(svg.with_suffix(".png"))], check=True, env=env)
        print(f"Generated {svg.name} and {svg.with_suffix('.png').name}")


def architecture():
    d = Drawing(1060, "01", "模块分层与依赖", "单个二进制 crate · 具体组件在 app 装配 · framework 保持通用")
    d.card(50, 150, 400, 135, "配置与入口", "main.rs / config.rs\nTOML + CLI · 信号与主机身份")
    d.card(550, 150, 840, 135, "应用装配  app.rs", "创建具体采集器、类型化 Key、组装器和输出\n注册 Schedule 与输出策略，最后启动 Runtime")
    d.arrow([(450, 216), (545, 216)], BLUE)
    d.arrow([(970, 285), (970, 315), (270, 315), (270, 370)], BLUE, dashed=True)
    d.arrow([(970, 315), (720, 315), (720, 370)], BLUE, dashed=True)
    d.arrow([(970, 315), (1170, 315), (1170, 370)], BLUE, dashed=True)
    d.text(64, 348, "虚线：创建与注册组件", 17, BLUE)
    d.card(50, 375, 430, 205, "采集层  collectors", "实现 Collector<Data = T>\n读取 /proc、sysfs、驱动命令\n维护差分基线与来源能力\n输出普通 Rust 领域数据", TEAL)
    d.card(505, 375, 430, 205, "组装层  assemblers", "实现 Assembler\n声明依赖，读取 Batch\n合并进程 / 线程实例与指标\n调用 protocol 编码消息", PURPLE)
    d.card(960, 375, 430, 205, "输出层  outputs", "实现 Output\nMCAP：压缩、分段、收尾\nFoxglove：SDK 实时发送\n接收已编码 Sample", AMBER)
    for x in (265, 720, 1175):
        d.arrow([(x, 580), (x, 652)], MUTED)
    d.text(280, 622, "依赖通用接口", 17, MUTED)
    d.card(50, 657, 1340, 165, "框架层  framework", "Collector / Assembler / Output · Key<T> / Batch / Sample\nRuntime：独立调度与观测触发    Dispatcher：有界队列与输出隔离\nOperationContext / Task：取消、截止时间、任务监督；不认识具体指标或 Protobuf", BLUE, "#EAF2FD")
    d.card(50, 858, 640, 130, "领域数据  data", "普通 Rust 类型，供 collectors / assemblers 引用\n数据结构独立于序列化和输出", TEAL, "#EAF7F4", 19)
    d.card(720, 858, 670, 130, "协议  protocol / proto", "组装器通过此模块构造 Protobuf，编码一次\nbuild.rs 生成类型和完整 descriptor", PURPLE, "#F3EFFA", 19)
    d.footer("实线：主要接口依赖；底部列出共享数据 / 协议模块。运行时数据流及线程边界见图 02。")
    return d


def flow():
    d = Drawing(1190, "02", "观测数据流与线程边界", "每次完成的观测逐条入队 · 只触发相关组装器 · 同一份 Protobuf 字节分发至独立输出")
    d.card(50, 150, 430, 125, "系统采集线程", "CPU + 各核 100 ms\n内存 200 ms", TEAL)
    d.card(505, 150, 430, 125, "进程 / 线程采集线程", "进程 CPU 100 ms · 内存 200 ms\n线程 CPU 100 ms", TEAL, size=19)
    d.card(960, 150, 430, 125, "GPU 采集线程", "系统利用率 100 ms · 内存 500 ms\n进程利用率 1000 ms", TEAL, size=18)
    for x in (265, 1175):
        d.arrow([(x, 275), (x, 307), (720, 307)], TEAL, end=False)
    d.arrow([(720, 275), (720, 340)], TEAL)
    d.card(390, 345, 660, 115, "观测 FIFO · 默认 1024 条", "满时暂停生产者；入队等待超时后停用来源", TEAL, "#EAF7F4")
    d.rect(50, 510, 1340, 391, "#EAF2FD", "#C2D5EE")
    d.text(575, 546, "Runtime 主循环 · 同步组装、编码、分发", 22, BLUE, bold=True)
    d.arrow([(720, 460), (720, 487), (265, 487), (265, 566)], BLUE)
    d.card(75, 571, 380, 142, "更新输入视图", "按依赖构造 Batch\n保留触发值，检查其他值是否过期", BLUE, size=18)
    d.card(525, 571, 390, 142, "消息组装", "System / Process Assembler\n实体合并，处理缺失与错误", PURPLE, size=18)
    d.card(985, 571, 380, 142, "Protobuf 编码一次", "protocol::encode\nSample：Bytes + descriptor", PURPLE, size=18)
    d.arrow([(455, 642), (520, 642)], BLUE)
    d.arrow([(915, 642), (980, 642)], BLUE)
    d.arrow([(1175, 713), (1175, 747), (720, 747), (720, 780)], PURPLE)
    d.card(390, 785, 660, 92, "Dispatcher · Arc<Sample>", "每 Topic 分配序号，分别入队，不执行输出 I/O", BLUE, size=18)
    d.arrow([(720, 877), (720, 928), (350, 928), (350, 965)], AMBER)
    d.arrow([(720, 928), (1090, 928), (1090, 965)], PURPLE)
    d.card(50, 970, 640, 155, "MCAP · 独立队列 + 工作线程", "All：FIFO 保留所有已接收消息\n默认 128 条；满时停用录制并尝试排空\nZstd 压缩 · 分段文件 · 完成时写索引", AMBER, "#FFF6E7", 19)
    d.card(750, 970, 640, 155, "Foxglove · 独立队列 + 工作线程", "Latest：按 Topic 合并待发值\n每 Topic 默认 100 ms 限频，默认队列 128 条\nFoxglove Rust SDK → WebSocket → 曲线", PURPLE, "#F3EFFA", 19)
    d.footer("周期为默认值，来源按配置 / 能力启用。队列有限；实时合并不影响录制，过载和失败按各输出策略处理。")
    return d


def gpu():
    d = Drawing(1180, "03", "Jetson GPU 能力探测", "面向 Orin / Thor 按可用接口选源 · 启动时分项判断能力 · 当前 nvidia-smi 使用 GPU index 0")
    d.card(50, 150, 1340, 115, "GpuSources::probe · 受监督的启动探测", "默认总预算 15000 ms；分项独立判断，探测过程顺序执行；关闭 GPU 时跳过全部探测", BLUE, "#EAF2FD")
    for x in (265, 720, 1175):
        d.arrow([(720, 265), (720, 290), (x, 290), (x, 322)], BLUE, dashed=True)
    lanes = ((50, "系统 GPU 利用率", TEAL), (505, "系统 / 进程 GPU 内存", AMBER), (960, "进程 GPU 利用率", PURPLE))
    for x, title, color in lanes:
        d.rect(x, 327, 430, 482, "#FFFFFF", "#CFDAE5")
        d.text(x + 24, 366, title, 24, color, bold=True)
    d.card(70, 393, 390, 85, "1  sysfs load", "显式路径或已知节点", TEAL, "#EAF7F4", 18)
    d.arrow([(265, 478), (265, 516)], TEAL)
    d.text(281, 505, "不可用", 16, MUTED)
    d.card(70, 521, 390, 85, "2  tegrastats", "持续子进程，解析 GR3D 百分比", TEAL, "#EAF7F4", 17)
    d.arrow([(265, 606), (265, 644)], TEAL)
    d.text(281, 633, "不可用", 16, MUTED)
    d.card(70, 649, 390, 85, "3  nvidia-smi", "查询系统 GPU 利用率", TEAL, "#EAF7F4", 18)
    d.text(75, 776, "选择首个有效来源", 19, TEAL, bold=True)
    d.card(525, 393, 390, 100, "1  nvmap clients", "内核 total + PID 分配统计\n保留 nvmap 统计口径", AMBER, "#FFF6E7", 18)
    d.arrow([(720, 493), (720, 531)], AMBER)
    d.text(736, 520, "不可用", 16, MUTED)
    d.card(525, 536, 390, 185, "2  nvidia-smi", "系统 memory.used / total\n进程 compute-apps.used_memory\n两项分别判断，任一有效可启用\n不以系统查询成功作为前提", AMBER, "#FFF6E7", 17)
    d.text(530, 758, "未支持的子项保留错误原因", 19, AMBER)
    d.text(530, 788, "进程子项仅在进程采集开启时探测", 17, MUTED)
    d.card(980, 393, 390, 120, "nvidia-smi pmon", "进程采集开启时独立探测\nSM 利用率，周期至少 1000 ms", PURPLE, "#F3EFFA", 18)
    d.arrow([(1175, 513), (1175, 551)], PURPLE)
    d.card(980, 556, 390, 152, "有效性与进程身份", "拒绝不支持或全 N/A 的活动记录\n空进程表允许后续继续采集\n运行期核对 PID + start_ticks", PURPLE, "#F3EFFA", 17)
    d.text(985, 758, "独立于 nvmap 和系统查询结果", 19, PURPLE)
    d.text(985, 788, "接口支持范围取决于驱动 / BSP", 17, MUTED)
    for x in (265, 720, 1175):
        d.arrow([(x, 809), (x, 841)], MUTED)
    d.card(50, 846, 430, 97, "类型化能力结果", "Capability<GpuData>", TEAL, size=18)
    d.card(505, 846, 430, 97, "类型化能力结果", "Capability<GpuMemoryData>", AMBER, size=18)
    d.card(960, 846, 430, 97, "类型化能力结果", "Capability<ProcessGpuUtilizationData>", PURPLE, size=17)
    d.card(50, 984, 650, 135, "成功：注册采集器", "按各自周期读取新观测；探测数据不发布\n普通读取错误可在下一周期重试", TEAL, "#EAF7F4", 19)
    d.card(730, 984, 660, 135, "失败：注册不可用输入", "不创建采集线程，本次运行不重复探测\n其他能力继续工作；修复后需重启", AMBER, "#FFF6E7", 19)
    d.footer("GPU 共享系统 RAM；驱动分配统计不等同于 GPU 独占物理显存。图示为实现逻辑，实体 Orin / Thor 仍需验证。")
    return d


def lifecycle():
    d = Drawing(1135, "04", "启动与退出生命周期", "先完成装配再启动采集 · 退出共用默认 5000 ms 预算 · 采集与输出分阶段收尾")
    d.text(50, 164, "启动阶段", 24, BLUE, bold=True)
    d.card(50, 190, 290, 136, "配置与身份", "加载 TOML / CLI\n读取 host / boot ID", BLUE, size=18)
    d.card(400, 190, 290, 136, "输出与 Runtime", "创建输出和观测队列\n注册基础采集器", BLUE, size=18)
    d.card(750, 190, 290, 136, "GPU 能力探测", "受启动预算监督\n注册能力与剩余组件", BLUE, size=18)
    d.card(1100, 190, 290, 136, "Runtime::run", "通知采集线程启动\n接收观测与监督任务", TEAL, size=18)
    for x in (340, 690, 1040):
        d.arrow([(x, 257), (x + 55, 257)], BLUE)
    d.text(50, 365, "采集线程注册后等待启动通知；GPU 启动探测使用单独的受监督任务。", 19, MUTED)
    d.arrow([(1245, 326), (1245, 413), (1020, 413)], TEAL)
    d.rect(50, 398, 950, 65, "#EAF2FD", "#C2D5EE")
    d.text(78, 440, "退出触发：Ctrl+C / SIGTERM · 运行时长到达 · 致命错误", 23, BLUE, bold=True)
    d.text(50, 526, "退出预算（默认值）", 24, BLUE, bold=True)
    d.text(50, 570, "t = 0 ms", 20, MUTED)
    d.text(720, 570, "最多 2500 ms", 20, MUTED, anchor="middle")
    d.text(1390, 570, "共同截止时间 5000 ms", 20, MUTED, anchor="end")
    d.rect(50, 590, 660, 60, "#CBEAE3", radius=9)
    d.rect(720, 590, 670, 60, "#E2D8F3", radius=9)
    d.text(380, 629, "取消采集 + 处理已完成观测", 24, TEAL, anchor="middle", bold=True)
    d.text(1055, 629, "输出排空 + 收尾", 24, PURPLE, anchor="middle", bold=True)
    d.card(50, 677, 660, 170, "阶段 A · 最多使用一半预算", "取消采集，继续消费 FIFO 并组装分发\n断开入口，结束采集任务等待\n未交付 / 未组装观测、未退出任务均报告错误", TEAL, size=20)
    d.card(730, 677, 660, 170, "阶段 B · 所有输出并行收尾", "同时关闭输出入队端，工作线程排空各自队列\nMCAP 写索引 / footer 并同步文件\nFoxglove 解除常规限频，排空并关闭服务", PURPLE, size=20)
    d.arrow([(710, 764), (725, 764)], MUTED)
    d.text(50, 889, "阶段 A 提早结束，剩余时间归输出使用；不会为每个输出重新开始一个完整等待预算。", 20, MUTED)
    d.card(50, 935, 1340, 133, "监督边界", "OperationContext / Task 监督工作任务；超时报告错误，不能强杀不合作线程或内核 I/O。\n组装器在主循环同步执行，应保持有界且无阻塞 I/O；预算不是对任意卡死代码的硬实时保证。", AMBER, "#FFF6E7", 20)
    d.footer("正常收尾才能完成当前 MCAP 分段；超时、强制终止或掉电不保证末尾数据与索引完整。")
    return d


def main():
    renderer = shutil.which("rsvg-convert")
    if renderer is None:
        raise SystemExit("rsvg-convert is required; install librsvg and a Chinese font first.")
    # Keep font caches outside the repository and writable in sandboxed environments.
    with tempfile.TemporaryDirectory(prefix="syslens-diagram-fonts-") as cache:
        env = dict(os.environ, XDG_CACHE_HOME=cache)
        for name, draw in (
            ("architecture-overview", architecture),
            ("observation-flow", flow),
            ("gpu-capabilities", gpu),
            ("shutdown-lifecycle", lifecycle),
        ):
            draw().save(name, renderer, env)


if __name__ == "__main__":
    main()
