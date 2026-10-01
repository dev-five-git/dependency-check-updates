const { describe, expect, test } = require("bun:test");
const { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");

const mainJs = join(__dirname, "..", "main.js");

async function run(main, root, args) {
  const child = Bun.spawn(["node", main, ...args], { cwd: root, stdout: "pipe", stderr: "pipe" });
  const [status, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  return { status, stdout, stderr };
}

describe("node CLI bridge (fixed metadata, no public registry)", () => {
  test("query is read-only and upgrade edits every Android declaration", async () => {
    const root = mkdtempSync(join(tmpdir(), "dcu-node-android-"));
    const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch(request) {
      if (new URL(request.url).pathname !== "/maven/sample/lib/maven-metadata.xml") return new Response("not found", { status: 404 });
      return new Response("<metadata><versioning><versions><version>1.0.0</version><version>1.1.0</version></versions></versioning></metadata>");
    }});
    const paths = ["apps/app/src-tauri/gen/android/app", "apps/app/src-tauri/gen/android/buildSrc"];
    const original = `repositories { maven { url = uri("http://127.0.0.1:${server.port}/maven") } }\r\nimplementation("sample:lib:1.0.0") // retained\r\n// implementation("sample:lib:0.0.0")\r\n`;
    try {
      for (const path of paths) { mkdirSync(join(root, path), { recursive: true }); writeFileSync(join(root, path, "build.gradle.kts"), original); }
      writeFileSync(join(root, "maven.json"), JSON.stringify({ schemaVersion: 1, repositories: [{ url: `http://127.0.0.1:${server.port}/maven` }] }));
      const args = ["-d", "sample:lib", "--maven-config", "maven.json", "--format", "json-report", "--fail-on-incomplete"];
      const query = await run(mainJs, root, args);
      expect(query.status, query.stderr).toBe(0);
      const report = JSON.parse(query.stdout);
      expect(report.items.length).toBe(2); expect(report.summary.updates).toBe(2);
      for (const path of paths) expect(readFileSync(join(root, path, "build.gradle.kts"), "utf8")).toBe(original);
      const applied = await run(mainJs, root, [...args, "-u"]);
      expect(applied.status, applied.stderr).toBe(0);
      expect(JSON.parse(applied.stdout).applyOutcome).toBe("committed");
      for (const path of paths) expect(readFileSync(join(root, path, "build.gradle.kts"), "utf8")).toBe(original.replace('implementation("sample:lib:1.0.0")', 'implementation("sample:lib:1.1.0")'));
    } finally { server.stop(true); rmSync(root, { recursive: true, force: true }); }
  }, 30000);

  test("cleanup failures are structured and produce a failure exit", async () => {
    const root = mkdtempSync(join(tmpdir(), "dcu-node-cleanup-"));
    try {
      writeFileSync(join(root, "package.json"), "{}");
      mkdirSync(join(root, "package-lock.json"));
      const result = await run(mainJs, root, ["--rm", "--format", "json-report"]);
      expect(result.status).toBe(1);
      expect(JSON.parse(result.stdout).diagnostics.some(d => d.code === "cleanup-failed")).toBe(true);
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
});
