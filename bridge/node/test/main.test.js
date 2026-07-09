const { describe, expect, test } = require("bun:test");
const { mkdtempSync, readFileSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { spawnSync } = require("node:child_process");

const repoPackageDir = join(__dirname, "..");
const mainJs = join(repoPackageDir, "main.js");

describe("node CLI bridge", () => {
  test("updates PEP 735 dependency-groups in pyproject.toml", () => {
    const root = mkdtempSync(join(tmpdir(), "dcu-node-pyproject-"));
    try {
      const projectDir = join(root, "py-test");
      require("node:fs").mkdirSync(projectDir);
      const pyprojectPath = join(projectDir, "pyproject.toml");

      writeFileSync(
        pyprojectPath,
        `[project]
name = "braillify-test"
version = "0.1.0"
description = ""
authors = [{ name = "owjs3901", email = "owjs3901@gmail.com" }]
readme = "README.md"
requires-python = ">=3.13"
dependencies = ["braillify"]

[tool.uv.sources]
braillify = { workspace = true }

[dependency-groups]
dev = ["pytest>=9.0.3"]
`,
      );

      const result = spawnSync("node", [mainJs, "-d", "-u", "--rm"], {
        cwd: root,
        encoding: "utf8",
      });

      expect(result.status).toBe(0);
      expect(`${result.stdout}${result.stderr}`).toContain("pytest");
      expect(readFileSync(pyprojectPath, "utf8")).toContain('dev = ["pytest>=9.1.1"]');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
