"use strict";

// Loads the correct per-platform prebuilt binary from an optional dependency
// like `@shared-nothing/shared-nothing-<platform>-<arch>`, then adds an
// ergonomic `get()` helper on NodeRef.
//
// Platforms currently published: linux-x64-gnu, linux-x32-gnu.
// Windows (msvc) and macOS (x64) binaries are produced and published by the
// CI release workflow (see .github/workflows/release.yml).

function platformTarget() {
  const { platform: p, arch: a } = process;
  if (p === "darwin") return `darwin-${a}`;
  if (p === "win32") return `win32-${a}-msvc`;
  if (p === "linux") {
    // Node reports 32-bit x86 as "ia32"; napi uses "x32".
    const arch = a === "ia32" ? "x32" : a;
    let libc = "gnu";
    try {
      const header = process.report && process.report.getReport() && process.report.getReport().header;
      // glibc is set on the report header for glibc systems, absent on musl.
      if (!(header && header.glibcVersionRuntime)) libc = "musl";
    } catch {
      libc = "musl";
    }
    return `linux-${arch}-${libc}`;
  }
  throw new Error(`shared-nothing: unsupported platform ${p}-${a}`);
}

const target = platformTarget();
let binding;
try {
  binding = require(`@shared-nothing/shared-nothing-${target}`);
} catch (e) {
  throw new Error(
    `shared-nothing: no prebuilt binary for platform target "${target}". ` +
      `Install the matching optional dependency, or run from source: ${e.message}`,
  );
}

// NodeRef convenience: return the child NodeRef or a materialized scalar.
if (binding.NodeRef && binding.NodeRef.prototype && !binding.NodeRef.prototype.get) {
  binding.NodeRef.prototype.get = function get(key) {
    const child = this.get_node(key);
    return child || this.get_value(key);
  };
}

module.exports = binding;