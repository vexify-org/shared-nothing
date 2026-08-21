import { test } from "node:test";
import assert from "node:assert";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const sn = require("../index.js");
const { SharedRegion } = sn;

function freshRegion(backend) {
  const id = `it-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  if (backend === "sab") {
    const sab = new SharedArrayBuffer(1 << 20);
    return SharedRegion.wrap(Buffer.from(sab));
  }
  return SharedRegion.create({ size: 1 << 20, id });
}

function exercise(region, name) {
  test(`${name}: primitives roundtrip`, () => {
    const root = region.root();
    root.set("int", 42);
    root.set("neg", -7);
    root.set("float", 3.14);
    root.set("bool", true);
    root.set("str", "hello");
    root.set("null", null);
    assert.equal(root.get("int"), 42);
    assert.equal(root.get("neg"), -7);
    assert.equal(root.get("float"), 3.14);
    assert.equal(root.get("bool"), true);
    assert.equal(root.get("str"), "hello");
    assert.equal(root.get("null"), null);
  });

  test(`${name}: nested object graph`, () => {
    const root = region.root();
    const user = region.createObject(8);
    user.set("name", "Alice");
    user.set("age", 30);
    root.set_node("user", user);

    const profile = region.createObject(8);
    profile.set("city", "sz");
    user.set_node("profile", profile);

    const got = root.get("user");
    assert.ok(got.isObject());
    assert.equal(got.get("name"), "Alice");
    assert.equal(got.get("profile").get("city"), "sz");
  });

  test(`${name}: arrays push/get/set`, () => {
    const root = region.root();
    const arr = region.createArray(8);
    root.set_node("arr", arr);
    assert.equal(arr.push(10), 1);
    assert.equal(arr.push(20), 2);
    arr.set(2, 30);
    assert.equal(arr.length(), 3);
    assert.equal(arr.get(0), 10);
    assert.equal(arr.get(2), 30);
    const got = root.get("arr");
    assert.ok(got.isArray());
    assert.equal(got.get(1), 20);
  });

  test(`${name}: map (k/v) set/get/has/keys`, () => {
    const root = region.root();
    const m = region.createMap(8);
    root.set_node("map", m);
    m.set("k1", 1);
    m.set("k2", "two");
    assert.equal(m.has("k1"), true);
    assert.equal(m.has("nope"), false);
    assert.equal(m.get("k2"), "two");
    assert.deepEqual(m.keys().sort(), ["k1", "k2"]);
  });

  test(`${name}: delete + length`, () => {
    const root = region.root();
    root.set("gone", 1);
    assert.equal(root.has("gone"), true);
    assert.equal(root.delete("gone"), true);
    assert.equal(root.has("gone"), false);
    assert.equal(root.get("gone"), null);
  });

  test(`${name}: atomic increment`, () => {
    const root = region.root();
    const stats = region.createObject(4);
    root.set_node("stats", stats);
    assert.equal(stats.increment("hits"), 1);
    assert.equal(stats.increment("hits"), 2);
    assert.equal(stats.get("hits"), 2);
  });
}

const mmapRegion = freshRegion("mmap");
exercise(mmapRegion, "mmap");
const sabRegion = freshRegion("sab");
exercise(sabRegion, "sab");

test("cleanup regions (unlink mmap names)", () => {
  mmapRegion.close();
  sabRegion.close();
});