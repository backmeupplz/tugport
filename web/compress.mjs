// Writes .br and .gz copies of the built text files; the panel serves them to browsers that accept them.
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";

const walk = (dir) => readdirSync(dir).flatMap((f) => (statSync(join(dir, f)).isDirectory() ? walk(join(dir, f)) : [join(dir, f)]));
for (const file of walk("dist").filter((f) => /\.(js|css|html|svg|json)$/.test(f))) {
  const data = readFileSync(file);
  if (data.length < 1024) continue;
  writeFileSync(`${file}.br`, brotliCompressSync(data, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }));
  writeFileSync(`${file}.gz`, gzipSync(data, { level: 9 }));
}
