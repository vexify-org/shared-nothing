// Multi-worker stress test: verifies (1) no lost updates on lock-free atomic
// increments and (2) no torn reads on multi-word (128-bit) values, across many
// cooperating worker threads sharing one region.
import { test } from "node:test";
import assert from "node:assert";
import { Worker } from "node:worker_threads";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
const sn = require("../index.js");
const { SharedRegion } = sn;

const pkg = dirname(dirname(fileURLToPath(import.meta.url)));
const workerFile = join(dirname(fileURLToPath(import.meta.url)), "workers", "worker.mjs");

const NWRITERS = 4;
const ITERS = 20000;
const NREADERS = 2;
const READ_ITERS = 20000;

const A = 1n << 100n;
const B = A + 1n;

function spawnWorker(data, sab) {
  const w = new Worker(workerFile, {
    workerData: {
      pkg,
      ...data,
      ...(sab ? { sab } : {}),
    },
  });
  return new Promise((resolve, reject) => {
    w.on("message", resolve);
    w.on("error", reject);
  });
}

async function runBackend(backend, makeRegion, sab) {
  const region = makeRegion();
  const root = region.root();
  root.increment("count"); // pre-create the counter node (adds 1)
  root.set("wide", A); // pre-create the wide node

  const shmId = backend === "mmap" ? region.id() : undefined;

  const jobs = [];
  for (let i = 0; i < NWRITERS; i++) {
    jobs.push(spawnWorker({ backend, shmId, role: "counter", iters: ITERS }, sab));
  }
  jobs.push(spawnWorker({ backend, shmId, role: "wide-writer", iters: ITERS, A, B }, sab));
  for (let i = 0; i < NREADERS; i++) {
    jobs.push(spawnWorker({ backend, shmId, role: "reader", iters: READ_ITERS, A, B }, sab));
  }

  // Also hammer the counter/wide from the main thread to increase contention.
  for (let i = 0; i < ITERS; i++) {
    root.increment("count");
    root.set("wide", i % 2 === 0 ? B : A);
  }

  const results = await Promise.all(jobs);

  return { region, root, results };
}

test("mmap backend: no lost updates + no torn reads across 8 workers", async () => {
  const total = 1 + NWRITERS * ITERS + ITERS; // pre-create + main + workers
  const { region, root, results } = await runBackend(
    "mmap",
    () => SharedRegion.create({ size: 4 << 20, id: `str-${Date.now()}-${Math.random().toString(16).slice(2)}` }),
  );
  assert.equal(root.get("count"), total, "atomic increments must never lose updates");

  for (const r of results) {
    if (r.role === "reader") {
      assert.equal(r.monotonicOK, true, "counter must be monotonic (no torn write)");
      assert.equal(r.wideOK, true, "128-bit value must never be observed torn");
    }
  }
  region.close();
});

test("sab backend: no lost updates + no torn reads across 8 workers", async () => {
  const sab = new SharedArrayBuffer(8 << 20);
  const total = 1 + NWRITERS * ITERS + ITERS;
  const { region, root, results } = await runBackend(
    "sab",
    () => SharedRegion.wrap(Buffer.from(sab)),
    sab,
  );
  assert.equal(root.get("count"), total);
  for (const r of results) {
    if (r.role === "reader") {
      assert.equal(r.monotonicOK, true);
      assert.equal(r.wideOK, true);
    }
  }
  region.close();
});