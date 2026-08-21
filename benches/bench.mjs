// Benchmark: shared-nothing lock-free shared-memory ops vs structured-clone
// deep-copy of an equivalent object graph. Run with `npm run bench`.
import { performance } from "node:perf_hooks";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const sn = require("../index.js");
const { SharedRegion } = sn;

const N = 200_000;

function benchSharedMemory() {
  const region = SharedRegion.create({ size: 8 << 20, id: `bench-${Date.now()}` });
  const root = region.root();
  const stats = region.createObject(8);
  region.root().set_node("stats", stats);
  root.increment("hits"); // warm up / create node

  performance.mark("s0");
  for (let i = 0; i < N; i++) stats.increment("hits");
  performance.mark("s1");
  const sec = (performance.measure("s", "s0", "s1").duration) / 1000;
  const incr = Math.round(N / sec);

  performance.mark("w0");
  for (let i = 0; i < N; i++) stats.set("k", (i & 0xffff)); // scalar set (in-place reuse)
  performance.mark("w1");
  const wsec = (performance.measure("w", "w0", "w1").duration) / 1000;
  const setW = Math.round(N / wsec);

  performance.mark("r0");
  let sink = 0;
  for (let i = 0; i < N; i++) sink += stats.get("hits");
  performance.mark("r1");
  const rsec = (performance.measure("r", "r0", "r1").duration) / 1000;
  const getR = Math.round(N / rsec);

  region.close();
  if (sink === 0) console.log("warm");
  return { incr, setW, getR };
}

function benchStructuredClone() {
  const obj = {
    stats: { hits: 0, k: 0 },
    user: { name: "Alice", tags: ["a", "b", "c"], meta: new Map([["x", 1]]) },
  };
  performance.mark("c0");
  for (let i = 0; i < 5000; i++) structuredClone(obj);
  performance.mark("c1");
  const sec = (performance.measure("c", "c0", "c1").duration) / 1000;
  return Math.round(5000 / sec);
}

const shared = benchSharedMemory();
const clone = benchStructuredClone();

console.log("shared-nothing (lock-free shared memory):");
console.log(`  atomic increment:  ${shared.incr.toLocaleString()} ops/sec`);
console.log(`  scalar set:        ${shared.setW.toLocaleString()} ops/sec`);
console.log(`  scalar get:        ${shared.getR.toLocaleString()} ops/sec`);
console.log("");
console.log(`structuredClone (deep copy of a small graph): ${clone.toLocaleString()} clones/sec`);
console.log("");
console.log(
  `Note: shared-nothing reads/writes the SAME object from any worker code without ` +
    `serialization; structuredClone copies data (plus any postMessage/IPC cost).`,
);