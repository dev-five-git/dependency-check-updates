// Prepare every package without publishing, then verify the exact tarballs
// handed to npm. Bun materializes workspace ranges; npm supplies OIDC.
const { execFileSync } = require("node:child_process");
const { mkdirSync, readFileSync, readdirSync, realpathSync } = require("node:fs");
const { basename, join, resolve } = require("node:path");
const packageArg = require("npm-package-arg");

const command = (args) => execFileSync(args[0], args.slice(1), { encoding: "utf8" });
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const destination = process.argv[2];
if (!destination) throw new Error("Usage: node scripts/pack.js <new output directory>");

command(["bun", "run", "prepublishOnly"]);
const root = readJson("package.json");
const nativeDirs = readdirSync("npm", { withFileTypes: true })
  .filter((entry) => entry.isDirectory()).map((entry) => join("npm", entry.name)).sort();
if (nativeDirs.length !== root.napi.targets.length) {
  throw new Error("Expected one native package for every configured N-API target");
}

const output = resolve(destination);
// Refuse stale output so a retry cannot publish an older tarball by accident.
mkdirSync(output);
const packed = [];
for (const dir of [...nativeDirs, "."]) {
  const manifest = readJson(join(dir, "package.json"));
  const native = dir !== ".";
  if (native && (manifest.name !== `${root.name}-${basename(dir)}`
      || manifest.version !== root.version
      || root.optionalDependencies?.[manifest.name] !== root.version)) {
    throw new Error(`Native package name/version mismatch: ${dir}`);
  }
  const packDir = join(output, native ? "native" : "cli");
  mkdirSync(packDir, { recursive: true });
  const filename = resolve(packDir, command([
    "bun", "pm", "pack", "--quiet", "--ignore-scripts", "--destination", packDir,
    // Node's native realpath expands Windows 8.3 aliases (e.g. RUNNER~1).
    // Bun cannot locate the workspace lockfile when --cwd contains an alias.
    "--cwd", realpathSync.native(resolve(dir)),
  ]).trim());
  const contents = command(["tar", "-xOf", filename, "package/package.json"]);
  const published = JSON.parse(contents);
  for (const section of ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"]) {
    for (const [name, range] of Object.entries(published[section] ?? {})) {
      // Use npm's parser so bare paths/archives cannot escape a prefix check.
      let spec;
      try { spec = packageArg.resolve(name, range); } catch {
        throw new Error(`Unresolved local dependency or invalid npm spec in ${manifest.name}: ${name}=${range}`);
      }
      if (spec.type === "file" || spec.type === "directory" || /^git\+file:/i.test(range)) {
        throw new Error(`Unresolved local dependency in ${manifest.name}: ${name}=${range}`);
      }
    }
  }
  const files = command(["tar", "-tf", filename]).split(/\r?\n/);
  const required = native
    ? [`${root.napi.binaryName}.${basename(dir)}.node`]
    : ["main.js", "index.js", "index.d.ts"];
  for (const file of required) {
    if (!files.includes(`package/${file}`)) throw new Error(`Missing ${file} in ${filename}`);
  }
  packed.push({ name: published.name, version: published.version, filename });
}
console.log(JSON.stringify(packed));
