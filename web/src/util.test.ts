// Run with `npm test` (Node's built-in runner strips the types).
import { test } from "node:test";
import assert from "node:assert/strict";
import { cidr, joinArgs, labelsText, matchServers, mergePatch, parseEnv, parseLabels, pinReasons, sentence, splitArgs, sumUsage } from "./util.ts";

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
  // Arrays of objects compare by content, whatever order their keys come in.
  const mounts = [{ name: "data", readonly: false, target: "/data", type: "volume" }];
  assert.deepEqual(mergePatch({ mounts }, { mounts: [{ type: "volume", name: "data", target: "/data", readonly: false }] }), {});
  assert.deepEqual(mergePatch({ mounts }, { mounts: [{ type: "volume", name: "data", target: "/data", readonly: true }] }), {
    mounts: [{ type: "volume", name: "data", target: "/data", readonly: true }],
  });
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

test("labels round-trip and reject entries without a key or value", () => {
  assert.deepEqual(parseLabels("disk=ssd, gpu=yes\nzone=a=b"), { disk: "ssd", gpu: "yes", zone: "a=b" });
  assert.deepEqual(parseLabels("  "), {});
  assert.equal(parseLabels("disk"), null);
  assert.equal(parseLabels("=ssd"), null);
  assert.equal(parseLabels("disk="), null);
  const l = { disk: "ssd", gpu: "yes" };
  assert.deepEqual(parseLabels(labelsText(l)), l);
  assert.equal(labelsText(undefined), "");
});

test("pin reasons follow the server's rules", () => {
  assert.deepEqual(pinReasons("app", { ports: [], mounts: [{ type: "file" }] }), []);
  assert.deepEqual(pinReasons("database", { mounts: [{ type: "volume" }] }), ["it is a database"]);
  assert.deepEqual(pinReasons("app", { ports: [{}], mounts: [{ type: "volume" }, { type: "bind" }], host_network: true, docker_socket: true }), [
    "it keeps data in a volume",
    "it mounts a folder from the server",
    "it publishes ports on the server",
    "it uses host networking",
    "it uses the Docker socket",
  ]);
});

test("placement matches online servers by id or name, region and every label", () => {
  const s = (id: string, status: string, region: string | null, labels: Record<string, string> = {}) => ({ id, name: `n-${id}`, local: id === "local", status, region, labels });
  const all = [s("local", "online", "eu", { disk: "ssd" }), s("b", "online", "us", { disk: "ssd", gpu: "yes" }), s("c", "offline", "eu"), s("d", "unreachable", "us")];
  const ids = (p: Parameters<typeof matchServers>[1]) => matchServers(all, p).map((x) => x.id);
  assert.deepEqual(ids({}), ["local", "b"]);
  assert.deepEqual(ids({ node: "n-b" }), ["b"]);
  assert.deepEqual(ids({ node: "b" }), ["b"]);
  assert.deepEqual(ids({ node: "c" }), []);
  assert.deepEqual(ids({ region: "eu" }), ["local"]);
  assert.deepEqual(ids({ labels: { disk: "ssd" } }), ["local", "b"]);
  assert.deepEqual(ids({ labels: { disk: "ssd", gpu: "yes" } }), ["b"]);
  assert.deepEqual(ids({ region: "us", labels: { gpu: "no" } }), []);
});

test("usage adds up over servers and skips servers without the service", () => {
  const s = (cpu: number, memory: number, restarts = 0) => ({ services: [{ id: "a", cpu, memory, restarts }, { id: "b", cpu: 50, memory: 9, restarts: 9 }] });
  assert.deepEqual(sumUsage([s(1.5, 100, 2), null, s(2, 50)], "a"), { cpu: 3.5, memory: 150, restarts: 2 });
  assert.equal(sumUsage([null, { services: [] }], "a"), null);
});
