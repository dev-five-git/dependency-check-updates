const { expect, test } = require("bun:test");
const { mkdtempSync, cpSync, readFileSync, writeFileSync, readdirSync, mkdirSync, rmSync } = require("node:fs");
const { join } = require("node:path");
const { tmpdir } = require("node:os");

async function command(args, cwd) {
  const child = Bun.spawn(args, { cwd, stdout: "pipe", stderr: "pipe", env: { ...process.env, NAPI_RS_ENFORCE_VERSION_CHECK: "1" } });
  const [status, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect(status, `${args.join(" ")}\n${stderr}\n${stdout}`).toBe(0);
  return stdout;
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
    writeFileSync(join(copy, "package.json"), JSON.stringify(config));
    await command(["bun", "x", "--no-install", "napi", "create-npm-dirs", "--cwd", copy], repo);
    await command(["bun", "x", "--no-install", "napi", "artifacts", "--cwd", copy, "--output-dir", "."], repo);
    const npm = process.platform === "win32" ? ["cmd.exe", "/d", "/c", "npm"] : ["npm"];
    const pack = async dir => JSON.parse(await command([...npm, "pack", "--ignore-scripts", "--json", "--pack-destination", root], dir))[0];
    const cli = await pack(copy);
    const native = await pack(join(copy, "npm", suffix));
    expect(cli.files.some(f => f.path === "main.js")).toBe(true);
    expect(native.files.some(f => f.path === binary)).toBe(true);
    const installed = join(root, "installed"); mkdirSync(installed);
    await command([...npm, "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--omit=dev", "--prefix", installed, join(root, cli.filename), join(root, native.filename)], installed);
    const installedMain = join(installed, "node_modules", "@dependency-check-updates", "cli", "main.js");
    expect(readdirSync(join(installed, "node_modules", "@dependency-check-updates", "cli")).some(f => f.endsWith(".node"))).toBe(false);
    const help = await command(["node", installedMain, "--help"], root);
    expect(help).toContain("--compatible");
    const report = JSON.parse(await command(["node", installedMain, "--local-tools", "node", "--reject", "node", "--format", "json-report"], root));
    expect(report.schemaVersion).toBe(2); expect(report.items).toEqual([]);
    expect(JSON.parse(readFileSync(join(installed, "node_modules", "@dependency-check-updates", `cli-${suffix}`, "package.json"), "utf8")).version).toBe(JSON.parse(readFileSync(join(copy, "package.json"), "utf8")).version);
  } finally { rmSync(root, { recursive: true, force: true }); }
}, 120000);
