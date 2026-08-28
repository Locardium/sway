// Bump version, commit, tag, build Windows app + Windows/Linux server, publish GitHub Release.
// Usage: node scripts/release-win.js 1.0.0-beta.1
//
// The tag / GitHub release name use this version as-is. WiX's msi target
// only accepts a numeric-only pre-release, so any non-numeric pre-release
// (e.g. "beta.1") is auto-reduced to its digits ("1") just for the version
// written into package.json / tauri.conf.json / Cargo.toml.
//
// If anything fails after the tag is pushed (build, missing binary, gh
// release), the tag is deleted locally and on origin so a retry starts
// clean. The version-bump commit is never rolled back — it's harmless on
// its own and reverting it automatically risks reverting other people's
// pushes in between.
const { execSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const label = process.argv[2];
if (!label || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$/.test(label)) {
  console.error("Usage: node scripts/release-win.js <version>  (e.g. 1.0.0-beta.1)");
  process.exit(1);
}

const [core, pre] = label.split(/-(.+)/);
const numericPre = pre ? pre.replace(/\D/g, "") || "1" : null;
const version = numericPre ? `${core}-${numericPre}` : core;
if (version !== label) {
  console.log(`msi needs a numeric-only pre-release: building as ${version} (release tag stays ${label})`);
}

const root = path.resolve(__dirname, "..");
const isPrerelease = label.includes("-");
const tag = `v${label}`;

function run(cmd) {
  console.log(`\n$ ${cmd}`);
  execSync(cmd, { cwd: root, stdio: "inherit" });
}

function localTagExists() {
  return execSync("git tag -l", { cwd: root })
    .toString()
    .split("\n")
    .includes(tag);
}

function rollbackTag() {
  console.error(`\nRolling back tag ${tag}...`);
  if (localTagExists()) {
    try {
      run(`git tag -d ${tag}`);
    } catch (e) {
      console.error(`Could not delete local tag: ${e.message}`);
    }
  }
  try {
    run(`git push origin :refs/tags/${tag}`);
  } catch (e) {
    console.error(`Could not delete remote tag (may not have been pushed yet): ${e.message}`);
  }
}

// 1. Refuse to run with uncommitted changes, so the release commit is clean.
const status = execSync("git status --porcelain", { cwd: root }).toString().trim();
if (status) {
  console.error("Working tree not clean. Commit or stash changes first:\n" + status);
  process.exit(1);
}

if (localTagExists()) {
  console.error(`Tag ${tag} already exists locally. Delete it first if you want to redo this release: git tag -d ${tag}`);
  process.exit(1);
}

// 2. Bump version across package.json / tauri.conf.json / Cargo.toml.
run(`node scripts/bump-version.js ${version}`);

// 3. Commit (if the bump actually changed anything) + push, before tagging.
run(`git add -A`);
const staged = execSync("git status --porcelain", { cwd: root }).toString().trim();
if (staged) {
  run(`git commit -m "chore: release ${tag}"`);
} else {
  console.log("\nVersion already at the target value, nothing to commit.");
}
run(`git push`);

// 4. Tag + push the tag. Everything from here on rolls the tag back on failure.
run(`git tag ${tag}`);
run(`git push origin ${tag}`);

try {
  // 5. Build Windows app bundle + Windows/Linux server binaries.
  run(`pnpm build:win`);
  run(`pnpm server:build-win`);
  run(`pnpm server:build-linux`);

  // 6. Find the generated .msi.
  const msiDir = path.join(root, "target/release/bundle/msi");
  const msiFile = fs.readdirSync(msiDir).find((f) => f.endsWith(".msi"));
  if (!msiFile) {
    throw new Error(`No .msi found in ${msiDir}`);
  }
  const msiPath = path.join(msiDir, msiFile);
  console.log(`\nFound bundle: ${msiPath}`);

  const serverWinPath = path.join(root, "target/release/sway-server.exe");
  if (!fs.existsSync(serverWinPath)) {
    throw new Error(`No server binary found at ${serverWinPath}`);
  }

  const serverLinuxPath = path.join(
    root,
    "target/x86_64-unknown-linux-musl/release/sway-server"
  );
  if (!fs.existsSync(serverLinuxPath)) {
    throw new Error(`No server binary found at ${serverLinuxPath}`);
  }

  // 7. Publish GitHub Release with the app + both server binaries attached.
  const prereleaseFlag = isPrerelease ? "--prerelease" : "";
  run(
    `gh release create ${tag} ` +
      `"${msiPath}" ` +
      `"${serverWinPath}#sway-server-windows.exe" ` +
      `"${serverLinuxPath}#sway-server-linux" ` +
      `--title "Sway ${tag}" ${prereleaseFlag} --notes "Sway ${tag}"`
  );

  console.log(`\nDone. Release ${tag} published.`);
} catch (e) {
  console.error(`\nRelease step failed: ${e.message}`);
  rollbackTag();
  console.error(
    "\nTag rolled back. The version bump/commit already pushed is left as-is - fix the issue and rerun."
  );
  process.exit(1);
}
