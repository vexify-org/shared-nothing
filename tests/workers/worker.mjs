// Worker entry used by the stress test. Selects behavior via workerData.role.
import { parentPort, workerData } from "node:worker_threads";
import { createRequire } from "node:module";
import { join } from "node:path";

const require = createRequire(import.meta.url);
const sn = require(join(workerData.pkg, "index.js"));

let region;
if (workerData.backend === "sab") {
  region = sn.SharedRegion.wrap(Buffer.from(workerData.sab));
} else {
  region = sn.SharedRegion.attach({ backend: "mmap" }, workerData.shmId);
}
const root = region.root();

function run() {
  switch (workerData.role) {
    case "counter": {
      let last = 0;
      for (let i = 0; i < workerData.iters; i++) {
        last = root.increment("count");
      }
      parentPort.postMessage({ role: "counter", last });
      break;
    }
    case "wide-writer": {
      const a = workerData.A;
      const b = workerData.B;
      for (let i = 0; i < workerData.iters; i++) {
        root.set("wide", i % 2 === 0 ? a : b);
      }
      parentPort.postMessage({ role: "wide-writer" });
      break;
    }
    case "reader": {
      const a = workerData.A;
      const b = workerData.B;
      let prev = -1;
      let monotonicOK = true;
      let wideOK = true;
      for (let i = 0; i < workerData.iters; i++) {
        const c = root.get("count");
        if (c !== null && Number.isSafeInteger(c)) {
          if (c < prev) monotonicOK = false;
          prev = c;
        }
        const w = root.get("wide");
        // The 128-bit value must always be exactly A or B — never a torn mix.
        if (w !== a && w !== b) wideOK = false;
      }
      parentPort.postMessage({
        role: "reader",
        monotonicOK,
        wideOK,
        lastSeen: prev,
      });
      break;
    }
    default:
      parentPort.postMessage({ role: "unknown" });
  }
}

run();