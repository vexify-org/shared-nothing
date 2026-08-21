# shared-nothing：基于 Rust(N-API) 的无序列化共享内存对象图

## 1. 摘要 (Summary)

在 `/workspace`（当前为空的 greenfield 项目 `shared-nothing`）中，用 **Rust + napi-rs** 实现一个比 `SharedArrayBuffer` 更强的共享内存区域：多个 Node.js `Worker` 线程可**无序列化、无数据竞争**地并发读写同一个复杂对象图（嵌套对象、数组、`Map`）。

- **内存后端**：默认 `mmap`（POSIX 命名共享内存），特殊参数 `backend: "sab"` 时改用 `SharedArrayBuffer`。
- **并发模型**：`CAS + 版本号`（一种 seqlock 风格的乐观并发）。读者无锁；同一节点的写者通过 CAS/版本串行化并乐观重试，不同节点写入可并行；基于 Rust 原子（`Acquire/Release`）满足无数据竞争。
- **交付物**：完整可生产 npm 包（napi 绑定、`.d.ts`、单元/集成/竞争压力/基准、英文 README、Apache-2.0）。
- **品牌元数据**：README 与 package.json 标注 `Powered by Vexify`、`Apache-2.0`、`npm`、`nodejs`。

## 2. 现状分析 (Current State)

- `/workspace/README.md` 仅含标题 `# shared-nothing`。
- `/workspace/LICENSE` 为 Apache-2.0。
- 无任何源码、无 Cargo 工程、无 package.json。此为全新工程。

## 3. 目标用户 API（决定 JS 侧语义）

```js
import { SharedRegion } from 'shared-nothing'

// ===== 主线程：创建（默认 mmap）=====
const region = SharedRegion.create({ size: 1 << 20, id: 'demo' }) // mmap 后端
const root = region.root()                       // 根容器 = NodeRef 代理
const user = root.get('user') ?? root.add('user', { name: 'Alice' })
user.set('name', 'Bob'); user.get('tags')?.push('x')
root.set('stats', region.make({ hits: 0 }))       // 原子自增见下
root.set('meta', region.makeMap([['k', 1], ['v', 2]]))

// 将相同物理内存递给 Worker（mmap 传 id 字符串）
worker.postMessage({ shmId: region.id })          // region.id 形如 /shared_nothing_demo.mem

// ===== Worker：附着（mmap）=====
const region = SharedRegion.attach(workerData.shmId)  // 各 isolate 各自 mmap，指向同一物理内存
const name = region.root().get('user').get('name')    // 无锁跨线程读
region.root().get('stats').increment('hits')          // 乐观 CAS 原子自增

// ===== SAB 后端（特殊参数切换）：JS 侧持有/传递 SAB 对象 =====
const sab = new SharedArrayBuffer(1 << 20)
const region = SharedRegion.wrap(sab)               // 主线程 + 各 Worker 传入同一 sab 对象
```

**语义要点**（写入 README 与 README-decision）：
- 容器引用（Object/Array/Map 的 `NodeRef`）== 一个 arena 内固定 `u64 handle`，**零拷贝**，不序列化。
- 原始值（number/boolean/null/string/bigint 尾部）在读写边界必然发生单值拷贝（JS 字符串本质无法零拷贝共享），**但不做对象图整体序列化**；复杂度 O(单值)。
- 字符串/不可变值节点一旦写出即视为不可变，后续通过父容器条目整体原子替换（handle 替换），因此不可变值节点本身无需版本锁。
- 每个 `get/set/delete/has/increment` 原子；多步操作默认非事务性。可选 `region.transaction(fn)`（粗粒度原子写锁）保证多键组更新原子性。
- 容量：容器创建时按 `capacity` 预留（`region.make(obj, {capacity})`）；写入超出容量返回错误。避免无锁就地扩容的竞态（见决策 5）。

## 4. 核心设计

### 4.1 内存后端 (src/backend.rs)

抽象 `Backend`，二者都产出统一的 **块基址 `base_ptr: *mut u8` + 可用字节数 capacity**，后续竞技场逻辑与后端无关。

- **Mmap（默认）**：`shm_open` + `mmap(MAP_SHARED)`，或用 `shared_memory` crate 的后端。主线程 `create` 时创建并初始化；返回 `id`（POSIX 名字，如 `/shared_nothing_<uuid>.mem`）。Worker 线程 `attach(id)` 在各自地址空间重新 `mmap` 同一命名段 → 各 isolate 不同基址、同一物理页。生命周期：IPC/信号量引用计数 + 退出时清理（命名 shm 需 unlink，进程退出自动清理）。
- **SAB**：`wrap(sab)` —— 通过 napi 取得 V8 的 backing-store 数据指针作为 `base_ptr`（`napi_get_sharedarraybuffer_info` / napi-rs 的 raw 数据访问；若 napi-rs 高层 API 不足，回落到 napi-sys 直接调用）。SAB 由 JS 主线程 `postMessage` 分发给 Worker，天然共享，支持 `Atomics`。

要点：addon 在 Worker 中会各自重新加载（独立 isolate），共享状态**不得**放在进程级全局，必须全部经由 `base_ptr` 指向的共享区域；每个 isolate 的 napi 对象例持有自己的映射/包装。

### 4.2 竞技场布局 (src/arena.rs)

```
[ 区域头 ]  [ 分配器区 ]  [ 节点区:  NodeRecord... ]
  区域头: magic(0x5A4E) | backend | capacity | bump_free_offset(AtomicUsize) | ...
  分配器: 原子 bump 指针（CAS 累加）+ 简单 free-list（原子 LIFO 栈，存释放的块偏移）
  对齐:   8 字节对齐，满足 AtomicU64 放置
```
- 头与 allocator 全部字段为 `Atomic*`，`Acquire/Release` 序。
- `alloc_aligned(size)`：先取 free-list 头 CAS pop；空则 CAS bump；并发安全。

### 4.3 对象图编码 (src/graph.rs)

- **Value handle = `u64`**，编解码于 JS 代理层：
  - 低 32 位 = 节点记录在区域内的偏移（NodeRecord offset）。
  - 高 32 位 = kind/类型标签。
- **容器节点 NodeRecord**（OBJECT=1 / ARRAY=2 / MAP=3；VALUE=4；FREE=0）：
  ```
  NodeHeader {
    magic:u32, tag:u8, flags:u8, reserved:u16,
    seqlock: AtomicU64,     // 见 4.4
    count:   AtomicU64,
    capacity:u32, payload_off:u32, payload_len:u32,
    // VALUE 节点: 内联存储标量 (i64/f64/bool/null) 或 指向字符串块的 {str_off,str_len}
  }
  ```
  - OBJECT：payload = `value_handle[capacity]`，key 为属性名（写成字符串 VALUE 块，存放于 key 表；v1 用线性数组 + count），见决策 6。
  - ARRAY：payload = `value_handle[capacity]`，`count` 为 length。
  - MAP：payload = `{key_handle, val_handle}` 槽数组 `[capacity]`，含 used 标记；find 为线性（界内 O(n)），见决策 6。
- **不可变值节点**（VALUE 字符串/bigint/…）创建时一次写入，发布时并入父容器条目替换，故**不需要** seqlock。

### 4.4 无数据竞争并发——seqlock + CAS + 版本号 (src/arena.rs::seqlock)

- **容器条目读（读者无锁）**：
  1. `s0 = seqlock.load(Acquire)`；若为奇数则退回重读（有写者进行中）。
  2. 读取所需 payload 条目（单个 `u64` 条目一次原子读；读 `count` 亦然）。
  3. `s1 = seqlock.load(Acquire)`；`s1 != s0` 或 `s1` 为奇数 → 重试（被并发写打断）。
  4. 否则读到的是一个完整一致快照。
- **容器条目写（写者请锁 + 乐观提交）**：
  1. 尝试取得写权：`CAS(seqlock, even=v, odd=v|1)`（奇数位=写者标志位）。失败 → backoff 重试或返回 `EAGAIN`。
  2. 对要修改的条目计算新 `value_handle`（新标量/新容器），`Release` 写条目。
  3. 提交：`seqlock.store(v+2, Release)`（回到偶数且版本 +2）。优化：可改用 `fetch_add(2, Release)`。
  4. 若需更新 `count`（push 等），在同一次写窗口内 `Release` 写 `count` → 读者见 `count` 与条目一致。
- **原子自增 `increment`**（体现 CAS+版本，读者无关）：
  `loop { 读(版本s0, 当前值val); 若返回＝偶数一致; CAS(seqlock: s0→奇数) 进入写窗; 写 val+1; 提交 s0+2 } `；CAS 失败则用新快照重试。完整 read-modify-write，无丢失更新。
- **内存序**：所有 `seqlock/count/bump/free` 用 `AtomicU64/AtomicUsize`；进入写窗前 `Acquire`，payload 写 `Release`，提交 `Release`，读者复读 `Acquire`。
- **Torn-read 防治**：单条 `u64` 条目本身原子；跨条目/`count` 集合读取以「前后版本一致且为偶数」为护栏，杜绝撕裂读。
- **ABA**：seqlock 为 u64，碰撞概率可忽略；`CAS` 提交目标是「偶数版本」，杜绝 ABA 型错配。
- **不同容器线程安全**：锁在 NodeRecord 各自 seqlock 上，天然隔离，可并行写不同节点。

### 4.5 napi 导出层 (src/lib.rs + JS 代理)

- `#[napi]` 导出：`SharedRegion.create({size,id,backend})`、`SharedRegion.attach(id, {backend})`、`SharedRegion.wrap(sab)`、`region.root()`、`region.make(obj,{capacity})`、`region.makeMap(entries,{capacity})`、`region.transaction(fn)`。
- `#[napi]` `NodeRef` 代理：`get(key/index)`、`set`、`add`、`delete`、`has`、`length`、`entries/keys/values`、`increment(path)`；类型判定 `isArray/isMap/isObject`。
- 每个 napi 调用编译/解码 JS 对象 ↔ arena（单值拷贝+容器 handle），直接驱动 4.4 的原子操作，天然无竞争。
- `index.js`：napi 加载器（`require` 编译产物 `.node`）暴露上层 API。

## 5. 文件级变更清单

| 文件 | 作用 | 要点 |
|---|---|---|
| `package.json` | npm 元数据 | `name:"shared-nothing"`、`main:"index.js"`、`types:"index.d.ts"`、`napi` 配置（`@napi-rs/cli`）、`scripts:{build,test,bench}`、keywords/license，`description` 标注 Powered by Vexify、Apache-2.0、nodejs |
| `Cargo.toml` | crate 清单 | `napi`(v2, features=["napi8"])、`napi-derive`、`shared_memory`(或自实现 linux/mac/win)、`libc`；`[profile.release]` |
| `build.rs` | 可选 napi build 辅助 | 若采用 napi-sys 回落时用 |
| `src/lib.rs` | napi 入口、模块装配 | `#[napi]` 导出全部 API；注册 `register_post_work` 清理逻辑 |
| `src/backend.rs` | 后端抽象 | `Backend::Mmap/Sab`，`create/attach/wrap`，统一 `base_ptr/capacity`，`id` 语义，清理/解除映射 |
| `src/arena.rs` | 区域头 + 分配器 + seqlock | bump+free-list 分配；seqlock 读写/写锁/提交；`alloc_aligned`；进出写窗的原子操作 |
| `src/graph.rs` | 对象图编码 | NodeKind、NodeHeader、handle 编解码、VALUE 标量/字符串块、OBJECT/ARRAY/MAPpayload 布局 |
| `src/api/*` | JS↔arena 编解码 + NodeRef | get/set/has/delete/increment/entries/transaction 的实现 |
| `index.js` | 加载器/上层 API | `require('./shared_nothing.<platform>.node')`，导出 `SharedRegion` |
| `index.d.ts` | 类型声明 | 完整类型签名 |
| `tests/*.mjs` | 集成 + 竞争压力 + 撕裂检测 | 见 §7 |
| `benches/*.mjs` | 基准 | 对比 structured-clone + postMessage |
| `README.md` | 英文文档（替换现有） | 用法、设计、并发保证、限制、**Powered by Vexify** |
| `LICENSE` | 已存在 | Apache-2.0（保留） |
| `.gitignore` | 忽略产物 | `target/`、`*.node`、node_modules |

## 6. 关键决策与假设

1. **并发原语**：OCaml 式 seqlock（写者 CAS 取奇数、读者复读偶数一致）+ 单 `u64` 条目原子。满足「CAS+版本号、可并发」。
2. **命名/品牌**：`Powered by Vexify` 作为 README footer 与 package.json `description` 的品牌行并入；不做更深入集成（workspace 无 Vexify 相关资源）。
3. **后端切换**：`backend:"mmap"` 默认；仅显式传 `backend:"sab"` 时使用 SharedArrayBuffer（`wrap`）。
4. **共享分发**：mmap 用 `id` 字符串跨 worker 传递；SAB 用同一个 SAB 对象 `postMessage`。
5. **容量/扩容**：容器按创建时 `capacity` 预留，写超容返回错误；v1 不做锁内就地扩容（避免无锁扩容撕裂）。文档声明该限制与推荐容量。自由增长场景划入非目标。
6. **键与线性查找**：OBJECT 属性名 / MAP key 用字符串 VALUE 块存放；v1 用线性槽 + used 标记（界内 O(n)），保证正确性、牺牲极端性能；文档标注为后续可优化点（hash 化）。
7. **多操作事务性**：默认单操作原子；`transaction` 提供可选粗粒度原子写锁。
8. **多平台**：首选 `shared_memory` crate 保证 Windows/macOS/Linux 一致性；本环境先验证 Linux x64。

## 7. 验证步骤

1. **构建**：`npm install` → `npm run build`（`napi build`），确认生成 `shared_nothing.<platform>.node`。
2. **Rust 单测**：`cargo test` —— seqlock 读写互斥/多读者、分配器并发、handle 编解码、`increment` 原子性。
3. **集成测试**：`node tests/integration.mjs` —— mmap 与 sab 两后端下 object/array/map/string/number roundtrip、嵌套、delete/has/length。
4. **多 Worker 竞争压力（核心正确性）**：`node tests/worker-stress.mjs`
   - 计数器测试：N 个 worker 各 `increment` M 次同一 `stats.hits`，断言 `hits === N*M` → 证明**无丢失更新**。
   - 撕裂检测：写者反复改写一个大数组；若干读者反复整体读取，每次读须通过 seqlock 一致性检查（版本一致）→ 证明**无撕裂读**、无数据竞争。
   - SAB 后端重复上述用例。
5. **基准**：`npm run bench` —— 共享内存往返 vs `worker.postMessage(structuredClone(data))` 对照，给出吞吐/延迟对比。
6. **内存/资源检查**：多 Worker 反复 create/attach/close，确认无泄漏、命名 shm 被清理。

## 8. 实施顺序

1. Cargo 工程 + `package.json` + napi 脚手架，空 addon 可 `napi build`。
2. `backend.rs`：mmap 后端 create/attach/wrap（含 id 语义）。
3. `arena.rs`：区域头 + 分配器 + seqlock 原语（带 Rust 单测）。
4. `graph.rs`：编码 + handle + VALUE/容器。
5. `api/*`：get/set/add/delete/increment/entries/transaction + `index.js`/`index.d.ts`。
6. 集成测试 + worker 竞争压力（丢失更新 / 撕裂检测）。
7. SAB 后端补齐与复用测试。
8. bench + README（英文，含 Powered by Vexify / Apache-2.0）+ `.gitignore` + 收尾清理。最终 `cargo test` + 全部 node 测试通过。