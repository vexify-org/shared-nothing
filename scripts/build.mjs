// Builds the native addon with cargo and copies it into the package as a
// platform-addressed `.node` binding.
import { execSync } from "node:child_process";
import { copyFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));

console.log("Building Rust addon (cargo build --release) ...");
execSync("cargo build --release", { cwd: root, stdio: "inherit" });
execSync("cargo test --release", { cwd: root, stdio: "inherit" });
execSync("cargo build --release", { cwd: root, stdio: "inherit" });

const src = join(root, "target", "release", "libshared_nothing.so");
const suffix = `${process.platform}-${process.arch}-gnu`;
const dst = join(root, `shared-nothing.${suffix}.node`);

if (!existsSync(src)) {
  console.error(`Expected cdylib at ${src}`);
  process.exit(1);
}
copyFileSync(src, dst);
console.log(`Wrote ${dst}`);