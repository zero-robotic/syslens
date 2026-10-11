import { Input } from "./types.ts";

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
type Variables = { pid?: number; tid?: number };

export const inputs = ["/syslens/processes"];
export const output = "/foxglove_script/syslens/process_curves";

function append(
  points: Point[],
  label: string,
  value: number | bigint | string | null | undefined,
  scale = 1,
): void {
  // Missing measurements must not become false zeroes; a measured zero is valid.
  if (value == undefined || value === "") return;
  const number = Number(value);
  if (Number.isFinite(number)) points.push({ label, value: number / scale });
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
  for (const process of event.message.processes ?? []) {
    if (pid !== 0 && process.pid !== pid) continue;
    // Include the boot/start identity: reordered arrays and reused PIDs stay separate.
    const label = `${process.name} [${process.instance_id}]`;
    append(result.cpu, label, process.cpu_percent);
    append(result.rss_mib, label, process.rss_bytes, 1048576);
    append(result.virtual_mib, label, process.virtual_bytes, 1048576);
    append(result.gpu_utilization, label, process.gpu?.utilization_percent);
    append(result.gpu_memory_mib, label, process.gpu?.memory_used_bytes, 1048576);
    for (const thread of process.threads ?? []) {
      if (tid !== 0 && thread.tid !== tid) continue;
      append(result.thread_cpu, `${process.name}/${thread.name} [${thread.instance_id}]`, thread.cpu_percent);
    }
  }
  return result;
}
