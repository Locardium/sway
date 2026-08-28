// Bump version, commit, tag, build Windows, publish GitHub Release.
// Usage: node scripts/release-win.js 1.0.0-beta.1
const { execSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$/.test(version)) {
  console.error("Usage: node scripts/release-win.js <version>  (e.g. 1.0.0-beta.1)");
  process.exit(1);
}

const root = path.resolve(__dirname, "..");
const isPrerelease = version.includes("-");
const tag = `v${version}`;

function run(cmd) {
  console.log(`\n$ ${cmd}`);
  execSync(cmd, { cwd: root, stdio: "inherit" });
}

// 1. Refuse to run with uncommitted changes, so the release commit is clean.
const status = execSync("git status --porcelain", { cwd: root }).toString().trim();
if (status) {
  console.error("Working tree not clean. Commit or stash changes first:\n" + status);
  process.exit(1);
}

// 2. Bump version across package.json / tauri.conf.json / Cargo.toml.
run(`node scripts/bump-version.js ${version}`);

// 3. Commit + tag + push.
run(`git add -A`);
run(`git commit -m "chore: release ${tag}"`);
run(`git tag ${tag}`);
run(`git push`);
run(`git push origin ${tag}`);

// 4. Build Windows bundle.
run(`pnpm build:win`);

// 5. Find the generated .msi.
const msiDir = path.join(root, "app/src-tauri/target/release/bundle/msi");
const msiFile = fs.readdirSync(msiDir).find((f) => f.endsWith(".msi"));
if (!msiFile) {
  console.error(`No .msi found in ${msiDir}`);
  process.exit(1);
}
const msiPath = path.join(msiDir, msiFile);
console.log(`\nFound bundle: ${msiPath}`);

// 6. Publish GitHub Release with the .msi attached.
const prereleaseFlag = isPrerelease ? "--prerelease" : "";
run(
  `gh release create ${tag} "${msiPath}" --title "Sway ${tag}" ${prereleaseFlag} --notes "Sway ${tag}"`
);

console.log(`\nDone. Release ${tag} published.`);
