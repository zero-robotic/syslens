import type { Input } from "./types.ts";

// Embedded in syslens.json. These derived topics exist only inside Foxglove.
type Point = { label: string; value: number };
type Curves = {
  cpu: Point[];
  rss_mib: Point[];
  virtual_mib: Point[];
  gpu_utilization: Point[];
  gpu_memory_mib: Point[];
  thread_cpu: Point[];
};
type Variables = {
  pid?: number;
  tid?: number;
  command_contains?: string;
  cpu_min_percent?: number;
  rss_min_mib?: number;
  virtual_min_mib?: number;
  gpu_min_percent?: number;
  gpu_memory_min_mib?: number;
  thread_cpu_min_percent?: number;
};

export const inputs = ["/syslens/processes"];
export const output = "/foxglove_script/syslens/process_curves";

function lowerLimit(value: number | undefined): number {
  const number = Number(value ?? 0);
  return Number.isFinite(number) && number >= 0 ? number : 0;
}

function append(
  points: Point[],
  label: string,
  value: number | bigint | string | null | undefined,
  scale = 1,
  minimum = 0,
): void {
  // Missing measurements must not become false zeroes; a measured zero is valid.
  if (value == undefined || value === "") return;
  const number = Number(value) / scale;
  if (Number.isFinite(number) && number >= minimum) points.push({ label, value: number });
}

export default function script(
  event: Input<"/syslens/processes">,
  variables: Variables,
): Curves {
  const result: Curves = {
    cpu: [], rss_mib: [], virtual_mib: [], gpu_utilization: [],
    gpu_memory_mib: [], thread_cpu: [],
  };
  const pid = Number(variables.pid ?? 0);
  const tid = Number(variables.tid ?? 0);
  const command = typeof variables.command_contains === "string" ? variables.command_contains : "";
  const cpuMin = lowerLimit(variables.cpu_min_percent);
  const rssMin = lowerLimit(variables.rss_min_mib);
  const virtualMin = lowerLimit(variables.virtual_min_mib);
  const gpuMin = lowerLimit(variables.gpu_min_percent);
  const gpuMemoryMin = lowerLimit(variables.gpu_memory_min_mib);
  const threadMin = lowerLimit(variables.thread_cpu_min_percent);
  for (const process of event.message.processes ?? []) {
    if (pid !== 0 && process.pid !== pid) continue;
    // The optional cast also permits replay of schemas recorded before argv was added.
    const commandLine = (process as typeof process & { command_line?: string }).command_line;
    if (command !== "" && !commandLine?.includes(command)) continue;
    // Include the boot/start identity: reordered arrays and reused PIDs stay separate.
    const label = `${process.name} [${process.instance_id}]`;
    append(result.cpu, label, process.cpu_percent, 1, cpuMin);
    append(result.rss_mib, label, process.rss_bytes, 1048576, rssMin);
    append(result.virtual_mib, label, process.virtual_bytes, 1048576, virtualMin);
    append(result.gpu_utilization, label, process.gpu?.utilization_percent, 1, gpuMin);
    append(result.gpu_memory_mib, label, process.gpu?.memory_used_bytes, 1048576, gpuMemoryMin);
    for (const thread of process.threads ?? []) {
      if (tid !== 0 && thread.tid !== tid) continue;
      append(result.thread_cpu, `${process.name}/${thread.name} [${thread.instance_id}]`, thread.cpu_percent, 1, threadMin);
    }
  }
  return result;
}
