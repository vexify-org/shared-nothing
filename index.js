"use strict";

// N-API binding loader + thin ergonomic wrappers.
//
// The native addon already exports the full API surface (SharedRegion and
// NodeRef classes) and is safe to use directly. A small `get()` helper is added
// on NodeRef so that reading a nested key transparently returns either a child
// NodeRef (containers) or a materialized scalar.

const { existsSync } = require("node:fs");
const path = require("node:path");

const candidates = [
  `shared-nothing.${process.platform}-${process.arch}-gnu.node`,
  `shared-nothing.${process.platform}-${process.arch}-musl.node`,
  `shared-nothing.${process.platform}-${process.arch}.node`,
];

let binding = null;
for (const name of candidates) {
  const p = path.join(__dirname, name);
  if (existsSync(p)) {
    binding = require(p);
    break;
  }
}
if (!binding) {
  throw new Error(
    "Native binding not found. Run `npm run build` first (see README).",
  );
}

// NodeRef convenience: return the child NodeRef or a materialized scalar.
if (binding.NodeRef && binding.NodeRef.prototype && !binding.NodeRef.prototype.get) {
  binding.NodeRef.prototype.get = function get(key) {
    const child = this.get_node(key);
    return child || this.get_value(key);
  };
}

// For the SharedArrayBuffer backend, pass a Buffer view over the SAB:
//   SharedRegion.wrap(Buffer.from(sab))
// This keeps the native `wrap` binding (which consumes a Buffer) untouched.

module.exports = binding;