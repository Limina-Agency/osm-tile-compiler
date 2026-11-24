// Decodes every .mlt tile in a directory with @maplibre/mlt, the decoder
// MapLibre GL JS uses, and writes <tile>.ts.json beside it in the MLT
// specification's fixture format, as the specification's own synthetic
// tests print it.
//
//   node decode-ts.mjs <DIR>
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { decodeTile } from "@maplibre/mlt";
import { featureTablesToFeatureCollection } from "@maplibre/mlt/dist/vector/featureTablesToGeoJson.js";

const dir = process.argv[2];
let tiles = 0;
let failed = 0;
for (const file of readdirSync(dir).filter((f) => f.endsWith(".mlt")).sort()) {
  const out = join(dir, file.replace(/\.mlt$/, ".ts.json"));
  try {
    const tables = decodeTile(readFileSync(join(dir, file)), undefined, false);
    writeFileSync(out, JSON.stringify(featureTablesToFeatureCollection(tables)));
    tiles++;
  } catch (error) {
    console.error(`${file}: ${error}`);
    writeFileSync(out, JSON.stringify({ features: [], error: String(error) }));
    failed++;
  }
}
console.log(`ts: ${tiles} tiles decoded, ${failed} failed`);
process.exitCode = failed ? 1 : 0;
