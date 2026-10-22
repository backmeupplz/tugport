// Run with `npm test` (Node's built-in runner strips the types).
import { test } from "node:test";
import assert from "node:assert/strict";
import { cidr, joinArgs, mergePatch, parseEnv, sentence, splitArgs } from "./util.ts";

test("merge patch keeps only changes and nulls removed keys", () => {
  const a = { replicas: 1, source: { type: "git", repo: "o/r", branch: "main" }, ports: [{ container: 80 }], user: "app" };
  const b = { replicas: 2, source: { type: "image", image: "nginx" }, ports: [{ container: 80 }], user: undefined };
  assert.deepEqual(mergePatch(a, b), {
    replicas: 2,
    source: { type: "image", repo: null, branch: null, image: "nginx" },
    user: null,
  });
  assert.deepEqual(mergePatch(a, structuredClone(a)), {});
  assert.deepEqual(mergePatch({ resources: { cpus: 1 } }, { resources: { cpus: null } }), { resources: { cpus: null } });
});

test(".env parsing", () => {
  const env = parseEnv('# c\nexport A=1\nB = "two words"\nC=\'x=y\'\nD="l1\\nl2"\n\nbad line\n=nokey');
  assert.deepEqual(env, { A: "1", B: "two words", C: "x=y", D: "l1\nl2" });
});

test("command splitting round-trips", () => {
  for (const args of [["sh", "-c", "echo 'hi' there"], ["node", "server.js"], [""], ["a b", "it's"]]) {
    assert.deepEqual(splitArgs(joinArgs(args)), args);
  }
  assert.deepEqual(splitArgs(' npm  run "start dev" '), ["npm", "run", "start dev"]);
});

test("small helpers", () => {
  assert.equal(cidr("10.0.0.7"), "10.0.0.7/32");
  assert.equal(cidr("2001:db8::1"), "2001:db8::1/128");
  assert.equal(cidr("10.0.0.0/8"), "10.0.0.0/8");
  assert.equal(sentence("wrong name or password"), "Wrong name or password.");
});
