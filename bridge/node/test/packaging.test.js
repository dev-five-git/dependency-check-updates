const { expect, test } = require("bun:test");
const { mkdtempSync, cpSync, readFileSync, writeFileSync, readdirSync, mkdirSync, rmSync } = require("node:fs");
const { delimiter, join } = require("node:path");
const { tmpdir } = require("node:os");

async function command(args, cwd, expectedStatus = 0) {
  const env = { ...process.env, NAPI_RS_ENFORCE_VERSION_CHECK: "1" };
  // Windows may name this variable Path. Duplicate Path/PATH keys cause Node's
  // subprocesses to lose the local N-API binary directory.
  const pathKey = Object.keys(env).find(key => key.toLowerCase() === "path") ?? "PATH";
  env[pathKey] = join(__dirname, "..", "node_modules", ".bin") + delimiter + (env[pathKey] ?? "");
  const child = Bun.spawn(args, { cwd, stdout: "pipe", stderr: "pipe", env });
  const [status, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect(status, `${args.join(" ")}\n${stderr}\n${stdout}`).toBe(expectedStatus);
  return expectedStatus === 0 ? stdout : stderr;
}

test("packed npm CLI loads its separately installed native platform package", async () => {
  const repo = join(__dirname, "..");
  const root = mkdtempSync(join(tmpdir(), "dcu-npm-pack-"));
  const copy = join(root, "package"); mkdirSync(copy);
  try {
    const suffix = { "win32-x64": "win32-x64-msvc", "darwin-x64": "darwin-x64", "darwin-arm64": "darwin-arm64", "linux-x64": "linux-x64-gnu" }[`${process.platform}-${process.arch}`];
    expect(suffix).toBeDefined();
    const binary = `dependency-check-updates.${suffix}.node`;
    for (const file of ["package.json", "main.js", "index.js", "index.d.ts", binary]) cpSync(join(repo, file), join(copy, file));
    // This smoke test packages the runner's real artifact, not absent foreign
    // targets. Release CI separately collects every configured target.
    const config = JSON.parse(readFileSync(join(copy, "package.json"), "utf8"));
    config.napi.targets = [{ "win32-x64-msvc": "x86_64-pc-windows-msvc", "darwin-x64": "x86_64-apple-darwin", "darwin-arm64": "aarch64-apple-darwin", "linux-x64-gnu": "x86_64-unknown-linux-gnu" }[suffix]];
    // A real workspace range must be materialized in the artifact npm publishes.
    writeFileSync(join(root, "package.json"), JSON.stringify({ private: true, workspaces: ["package", "fixture"] }));
    mkdirSync(join(root, "fixture"));
    writeFileSync(join(root, "fixture", "package.json"), JSON.stringify({ name: "@dcu-test/fixture", version: "1.2.3" }));
    config.devDependencies = { "@dcu-test/fixture": "workspace:^" };
    writeFileSync(join(copy, "package.json"), JSON.stringify(config));
    await command(["bun", "install", "--lockfile-only", "--ignore-scripts"], root);
    await command(["bun", "x", "--no-install", "napi", "create-npm-dirs", "--cwd", copy], repo);
    await command(["bun", "x", "--no-install", "napi", "artifacts", "--cwd", copy, "--output-dir", "."], repo);
    const npm = process.platform === "win32" ? ["cmd.exe", "/d", "/c", "npm"] : ["npm"];
    const packed = JSON.parse(await command(["node", join(repo, "scripts", "pack.js"), join(root, "packed")], copy));
    const cli = packed.find(p => p.name === "@dependency-check-updates/cli");
    const native = packed.find(p => p.name === `@dependency-check-updates/cli-${suffix}`);
    expect(packed.length).toBe(2);
    const manifest = JSON.parse(await command(["tar", "-xOf", cli.filename, "package/package.json"], root));
    expect(manifest.devDependencies["@dcu-test/fixture"]).toBe("^1.2.3");
    expect(manifest.optionalDependencies[native.name]).toBe(config.version);
    const local = JSON.parse(readFileSync(join(copy, "package.json"), "utf8"));
    for (const [index, range] of ["file:../fixture", "../fixture"].entries()) {
      local.devDependencies["@dcu-test/fixture"] = range;
      writeFileSync(join(copy, "package.json"), JSON.stringify(local));
      expect(await command(["node", join(repo, "scripts", "pack.js"), join(root, `rejected-${index}`)], copy, 1)).toContain("Unresolved local dependency");
    }
    const installed = join(root, "installed"); mkdirSync(installed);
    await command([...npm, "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--omit=dev", "--prefix", installed, cli.filename, native.filename], installed);
    const installedMain = join(installed, "node_modules", "@dependency-check-updates", "cli", "main.js");
    expect(readdirSync(join(installed, "node_modules", "@dependency-check-updates", "cli")).some(f => f.endsWith(".node"))).toBe(false);
    const help = await command(["node", installedMain, "--help"], root);
    expect(help).toContain("--compatible");
    const report = JSON.parse(await command(["node", installedMain, "--local-tools", "node", "--reject", "node", "--format", "json-report"], root));
    expect(report.schemaVersion).toBe(2); expect(report.items).toEqual([]);
    expect(JSON.parse(readFileSync(join(installed, "node_modules", "@dependency-check-updates", `cli-${suffix}`, "package.json"), "utf8")).version).toBe(JSON.parse(readFileSync(join(copy, "package.json"), "utf8")).version);
  } finally { rmSync(root, { recursive: true, force: true }); }
}, 120000);
