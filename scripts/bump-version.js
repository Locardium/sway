// Bump version across every file that tracks the app release version.
// Usage: node scripts/bump-version.js 1.2.0
const fs = require("fs");
const path = require("path");

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$/.test(version)) {
  console.error("Usage: node scripts/bump-version.js <major.minor.patch>");
  process.exit(1);
}

const root = path.resolve(__dirname, "..");

function bumpJson(relPath) {
  const file = path.join(root, relPath);
  const json = JSON.parse(fs.readFileSync(file, "utf8"));
  const old = json.version;
  json.version = version;
  fs.writeFileSync(file, JSON.stringify(json, null, 2) + "\n");
  console.log(`${relPath}: ${old} -> ${version}`);
}

function bumpCargoToml(relPath) {
  const file = path.join(root, relPath);
  const text = fs.readFileSync(file, "utf8");
  const match = text.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) throw new Error(`no version field found in ${relPath}`);
  const updated = text.replace(/^version\s*=\s*"[^"]+"/m, `version = "${version}"`);
  fs.writeFileSync(file, updated);
  console.log(`${relPath}: ${match[1]} -> ${version}`);
}

bumpJson("package.json");
bumpJson("app/package.json");
bumpJson("app/src-tauri/tauri.conf.json");
bumpCargoToml("app/src-tauri/Cargo.toml");

console.log("\nDone. Review the diff, then commit.");
