// Type definitions for shared-nothing

/** A live handle into a shared object-graph node (container). */
export interface NodeRef {
  typeName(): string;
  isObject(): boolean;
  isArray(): boolean;
  isMap(): boolean;
  length(): number;

  /** Returns the child NodeRef if `key` holds a container, else the materialized scalar. */
  get(key: string | number): NodeRef | unknown;
  /** Materialized scalar (null for containers / missing keys). */
  getValue(key: string | number): unknown;
  /** The child container at `key`, or null. */
  getNode(key: string | number): NodeRef | null;

  has(key: string | number): boolean;
  /** Write a scalar (number/boolean/null/string/bigint). */
  set(key: string | number, value: unknown): void;
  /** Link a child container into `key`. */
  setNode(key: string | number, child: NodeRef): void;

  /** Append a scalar; returns the new length. Arrays only. */
  push(value: unknown): number;
  /** Append a child container; returns the new length. Arrays only. */
  pushNode(child: NodeRef): number;

  /** Lock-free atomic increment of the INT counter under `key`; returns new value. */
  increment(key: string): number;
  delete(key: string | number): boolean;
  keys(): string[];
}

export interface CreateOptions {
  size?: number;
  id?: string;
  backend?: "mmap" | "sab";
}

export interface AttachOptions {
  backend?: "mmap";
}

export const SharedRegion: {
  /** Create a shared region (default backend: OS mmap shared memory). */
  create(opts?: CreateOptions): SharedRegion;
  /** Attach (reopen) an existing mmap region by its id. */
  attach(opts: AttachOptions, id: string): SharedRegion;
  /** Wrap a SharedArrayBuffer (or Buffer) as the region's backing store. */
  wrap(arrayBuffer: SharedArrayBuffer | ArrayBuffer | Uint8Array): SharedRegion;
};

export interface SharedRegion {
  root(): NodeRef;
  createObject(capacity?: number): NodeRef;
  createArray(capacity?: number): NodeRef;
  createMap(capacity?: number): NodeRef;
  base(): number;
  capacity(): number;
  id(): string;
}

export const NodeRef: { prototype: NodeRef };