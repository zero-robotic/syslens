import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import script from "./process_curves.ts";

const mib = 1048576;
const worker = {
  pid: 42, name: "python3", instance_id: "boot:42:9",
  command_line: "python3 worker.py --mode test", cpu_percent: 5,
  rss_bytes: 100 * mib, virtual_bytes: String(200 * mib),
  gpu: { utilization_percent: 7, memory_used_bytes: BigInt(20 * mib) },
  threads: [{ tid: 43, name: "worker", instance_id: "boot:42:9:43:10", cpu_percent: 3 }],
};
const run = (variables = {}, processes = [worker]) => script({ message: { processes } }, variables);
const values = (curves) => Object.fromEntries(Object.entries(curves).map(([key, points]) => [key, points.map(p => p.value)]));

test("inclusive independent thresholds compare values in the displayed units", () => {
  const limits = {
    cpu_min_percent: 5, rss_min_mib: 100, virtual_min_mib: 200,
    gpu_min_percent: 7, gpu_memory_min_mib: 20, thread_cpu_min_percent: 3,
  };
  assert.deepEqual(values(run(limits)), {
    cpu: [5], rss_mib: [100], virtual_mib: [200],
    gpu_utilization: [7], gpu_memory_mib: [20], thread_cpu: [3],
  });
  const fields = ["cpu", "rss_mib", "virtual_mib", "gpu_utilization", "gpu_memory_mib", "thread_cpu"];
  Object.entries(limits).forEach(([variable, minimum], index) => {
    const result = run({ ...limits, [variable]: minimum + 0.01 });
    fields.forEach((field, fieldIndex) => assert.equal(result[field].length, fieldIndex === index ? 0 : 1));
  });
});

test("command substring, PID and TID conditions compose without coupling metric thresholds", () => {
  assert.equal(run({ command_contains: "--mode test", pid: 42, tid: 43 }).thread_cpu.length, 1);
  assert.equal(run({ command_contains: "--mode test", pid: 42, tid: 99 }).cpu.length, 1);
  assert.equal(run({ command_contains: "--mode test", tid: 99 }).thread_cpu.length, 0);
  for (const variables of [{ command_contains: "Python3" }, { command_contains: "not-present" }, { pid: 99 }]) {
    assert.ok(Object.values(run(variables)).every(points => points.length === 0));
  }
  const result = run({ command_contains: "python3", cpu_min_percent: 6, rss_min_mib: 100 });
  assert.equal(result.cpu.length, 0);
  assert.equal(result.rss_mib.length, 1);
  assert.equal(result.thread_cpu.length, 1);
});

test("old schemas, empty command lines and read failures only fail an active command filter", () => {
  for (const extra of [
    { command_line: undefined }, { command_line: "" },
    { command_line: undefined, command_line_error: "Permission denied" },
  ]) {
    const processes = [{ ...worker, ...extra }];
    assert.equal(run({}, processes).cpu.length, 1);
    assert.equal(run({ command_contains: "" }, processes).cpu.length, 1);
    assert.ok(Object.values(run({ command_contains: "python3" }, processes)).every(points => points.length === 0));
  }
});

test("valid zeroes survive default/invalid thresholds; missing and nonfinite data never become zero", () => {
  const zero = { ...worker, cpu_percent: 0, rss_bytes: 0n, gpu: {} };
  for (const minimum of [undefined, 0, -1, NaN, Infinity]) {
    const result = run({ cpu_min_percent: minimum }, [zero]);
    assert.equal(result.cpu[0].value, 0);
    assert.equal(result.rss_mib[0].value, 0);
    assert.deepEqual(result.gpu_utilization, []);
    assert.deepEqual(result.gpu_memory_mib, []);
  }
  for (const value of [undefined, null, "", NaN, Infinity]) {
    assert.deepEqual(run({}, [{ ...worker, cpu_percent: value }]).cpu, []);
  }
  assert.ok(Object.values(run({}, [])).every(points => points.length === 0));
});

test("filtering follows each sample and retains identity across array reorder and PID reuse", () => {
  const reused = { ...worker, instance_id: "boot:42:100", cpu_percent: 6 };
  const first = run({ cpu_min_percent: 5 }, [worker, reused]).cpu;
  const reordered = run({ cpu_min_percent: 5 }, [reused, worker]).cpu;
  assert.deepEqual(reordered, [...first].reverse());
  assert.notEqual(first[0].label, first[1].label);
  assert.equal(run({ cpu_min_percent: 5 }, [{ ...worker, cpu_percent: 4 }]).cpu.length, 0);
  assert.equal(run({ cpu_min_percent: 5 }, [worker]).cpu.length, 1);
});

test("importable layout embeds the tested script and exposes every filter with neutral defaults", () => {
  const layout = JSON.parse(readFileSync(new URL("./syslens.json", import.meta.url), "utf8"));
  assert.equal(layout.userNodes["syslens-process-curves"].sourceCode, readFileSync(new URL("./process_curves.ts", import.meta.url), "utf8"));
  assert.deepEqual(layout.globalVariables, {
    pid: 0, tid: 0, command_contains: "", cpu_min_percent: 0, rss_min_mib: 0,
    virtual_min_mib: 0, gpu_min_percent: 0, gpu_memory_min_mib: 0, thread_cpu_min_percent: 0,
  });
  const panels = Object.values(layout.configById);
  assert.equal(panels.filter(panel => panel.paths).length, 17);
  for (const variable of Object.keys(layout.globalVariables)) {
    assert.ok(layout.configById["Markdown!syslens-help"].markdown.includes(variable));
  }
});
