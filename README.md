# shared-nothing

**Serialization-free, lock-free shared-memory object graphs for Node.js worker threads, written in Rust (N-API).**

`Worker` threads in Node.js normally share **nothing**: to move a value between
threads you must `postMessage` it and pay for structured-clone serialization,
or manually juggle a raw `SharedArrayBuffer`. `shared-nothing` is like
`SharedArrayBuffer`, but *object-aware*: multiple workers can concurrently read
and write the **same complex object graph** — nested objects, arrays and `Map`s —
in shared memory, **without serializing anything** and **without data races**.

> Powered by Vexify · Apache-2.0 · npm · nodejs

---

## Highlights

- **No serialization.** Containers live in a native, off-heap arena. Reading a
  nested key yields a zero-copy handle to the real shared node — nothing is
  copied except the primitive value you read out.
- **No data races.** Node reads are lock-free and never block; writes use a
  CAS + version (seqlock) protocol. Single mutations are atomic and
  linearizable. `increment()` is a lock-free read-modify-write.
- **This is not a copy.** Two worker threads that `attach()` the same region
  see the same bytes; an update in one thread is instantly visible to the
  others.
- **Two backends.**
  - `mmap` (default): a POSIX named shared-memory segment, mapped by every
    thread.
  - `sab`: pass a `SharedArrayBuffer` (special parameter `backend: "sab"`).
- **Typed values**: numbers (`i64`/`f64`), booleans, `null`, strings,
  128-bit `BigInt`, plus nested `Object` / `Array` / `Map`.

## Install & build

Requires Rust 1.70+, Node.js 18+ and a C toolchain (`clang`/`gcc`, `cc`).

```bash
npm install          # nothing to install beyond dev tooling
npm run build        # cargo build --release → shared-nothing.<platform>.node
npm test             # unit + integration + multi-worker stress tests
npm run bench        # throughput comparison vs structuredClone
```

## Quick start

```js
const { SharedRegion } = require("shared-nothing");

// Main thread: create the shared region (mmap backend by default).
const region = SharedRegion.create({ size: 1 << 20 });
const root = region.root();

root.set("user", "alice");
root.increment("hits");                   // lock-free atomic counter
const user = region.createObject(8);
user.set("name", "Alice");
root.set_node("user", user);              // link a nested container (zero-copy)

// Hand the id to a worker:
const { Worker } = require("node:worker_threads");
const worker = new Worker(`
  const { parentPort, workerData } = require("node:worker_threads");
  const { SharedRegion } = require("/path/to/shared-nothing");
  const region = SharedRegion.attach({ backend: "mmap" }, workerData.id);
  const root = region.root();
  root.set("from_worker", root.increment("hits"));
  parentPort.postMessage("done");
`, { eval: true, workerData: { id: region.id() } });
```

### Worker case study

In a worker (or the main thread) a `NodeRef` returned by `get()` is the same
underlying node that other threads mutate — no copy, no message:

```js
// worker A
const region = SharedRegion.attach({ backend: "mmap" }, workerData.id);
const root = region.root();
root.get("user").set("name", "Bob");

// worker B (concurrently)
const root = SharedRegion.attach({ backend: "mmap" }, workerData.id).root();
console.log(root.get("user").get("name")); // "Bob" (eventually / atomically per op)
```

## API

### `SharedRegion`

| Method | Description |
| --- | --- |
| `create({ size?, id?, backend? })` | Create a region (default `mmap`). `size` bytes, default 1 MiB. |
| `attach(opts, id)` | Reopen an existing mmap region by its id. |
| `wrap(arrayBuffer)` | Back the region with a `SharedArrayBuffer` (`Buffer.from(sab)`). |
| `root()` | The root container (an object). |
| `createObject(cap?)` / `createArray(cap?)` / `createMap(cap?)` | Allocate a container node. |
| `id()` | Shared-memory id (pass to workers). |
| `capacity()` / `base()` | Region size / base address. |
| `close()` | Unlink the mmap name after all workers have attached. |

### `NodeRef`

| Method | Description |
| --- | --- |
| `get(key)` | Child `NodeRef` (container) or materialized scalar. |
| `getValue(key)` / `getNode(key)` | Scalar vs. container access. |
| `set(key, value)` / `set_node(key, nodeRef)` | Write a scalar / link a container. |
| `push(value)` / `pushNode(nodeRef)` | Append to an array. |
| `increment(key)` | Lock-free atomic increment of an `i64` counter. |
| `get`/`set` by index (`number` key) | Indexed access on arrays. |
| `has(key)`, `delete(key)`, `keys()`, `length()` | Introspection. |
| `isObject()` / `isArray()` / `isMap()` / `typeName()` | Container kind. |

`Map` is stored as key/value slots (`set`/`get`/`has`/`keys`). `Object` uses the
same storage, so keys are strings.

## Concurrency model (how "no data races" is guaranteed)

Every node carries a **seqlock** — a `u64` version whose low bit marks an
active writer.

- **Readers** (lock-free): snapshot the version, read, then re-check the
  version; retry if it changed or is odd. They never block and never observe a
  torn value (single-word values are read atomically; multi-word strings /
  `BigInt` are assembled only when the version is stable).
- **Writers**: claim the write window with a **CAS on the version**, mutate
  under it, then commit with an even version bump. Only one writer is inside a
  node at a time; different nodes are written in parallel.
- **`increment()`**: a full read-modify-write at the value node, so concurrent
  increments never lose an update.
- Memory ordering uses `Acquire`/`Release`, satisfying the Rust memory model.

The multi-worker stress test (`tests/worker-stress.mjs`) proves this: several
writers and readers hammer one counter and one 128-bit value across 8 worker
threads, and asserts **no lost updates** and **no torn reads**.

## Limitations (design decisions)

- **Capacity is fixed per container.** A node reserves `capacity` slots at
  creation. Writing past it returns an error (protected by the seqlock to avoid
  lock-free resize races). Pick `capacity` when you create a container.
  Unbounded live growth is out of scope for now.
- **Keys and linear scans.** Object/Map keys are matched by a linear scan over
  slots (O(n) within capacity). Correctness first; a hashed index is future
  work.
- **Whole-region snapshots** are not atomic across elements. Each single
  read/write is atomic; multi-step operations need your own coordination (a
  global lock is not exposed yet).
- **One process.** `mmap` shares memory within a process; the OS cleans the
  segment at exit. Keep the creating region alive while workers attach.
- **SAB backend** keeps whatever JS object holds the buffer alive for the
  region's lifetime.

## Benchmark (on this machine)

```
shared-nothing (lock-free shared memory):
  atomic increment:  ~4.7M ops/sec
  scalar set:        ~3.7M ops/sec
  scalar get:        ~2.8M ops/sec
structuredClone (deep copy of a small graph): ~293k clones/sec
```

`shared-nothing` never copies the object and works across threads — the
comparison is apples to the same object, without serialization.

## License

Apache-2.0. See [LICENSE](./LICENSE).

---

*Powered by Vexify.*