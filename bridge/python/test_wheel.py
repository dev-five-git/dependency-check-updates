"""Build/install smoke test: only local wheel archives and fixed HTTP metadata."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Metadata(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"<metadata><versioning><versions><version>1.0.0</version><version>1.1.0</version></versions></versioning></metadata>"
        self.send_response(200 if self.path == "/maven/sample/lib/maven-metadata.xml" else 404)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def smoke(wheel_dir):
    wheels = list(wheel_dir.glob("*.whl"))
    if len(wheels) != 1:
        raise RuntimeError("expected exactly one built wheel")
    with tempfile.TemporaryDirectory(prefix="dcu-wheel-") as directory:
        root = Path(directory)
        environment = root / "env"
        subprocess.run(["uv", "venv", "--no-project", str(environment)], check=True)
        python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        subprocess.run(["uv", "pip", "install", "--python", str(python), "--no-index", "--no-deps", str(wheels[0].resolve())], check=True)
        project = root / "project"
        project.mkdir()
        scripts = environment / ("Scripts" if os.name == "nt" else "bin")
        server = ThreadingHTTPServer(("127.0.0.1", 0), Metadata)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            repo = f"http://127.0.0.1:{server.server_port}/maven"
            config = project / "maven.json"
            config.write_text(json.dumps({"schemaVersion": 1, "repositories": [{"url": repo}]}))
            target = project / "build.gradle.kts"
            original = f'repositories {{ maven {{ url = uri("{repo}") }} }}\r\nimplementation("sample:lib:1.0.0") // retained\r\n'
            target.write_bytes(original.encode())
            for alias in ["dcu", "dependency-check-updates"]:
                executable = scripts / (alias + ".exe" if os.name == "nt" else alias)
                help_text = subprocess.run([str(executable), "--help"], cwd=project, check=True, capture_output=True, encoding="utf-8", timeout=30).stdout
                assert "--compatible" in help_text
                command = [str(executable), "--maven-config", str(config), "--format", "json-report", "--fail-on-incomplete"]
                report = json.loads(subprocess.run(command, cwd=project, check=True, capture_output=True, encoding="utf-8", timeout=30).stdout)
                assert report["items"][0]["latest"] == "1.1.0"
                assert target.read_bytes() == original.encode()
            report = json.loads(subprocess.run(command + ["-u"], cwd=project, check=True, capture_output=True, encoding="utf-8", timeout=30).stdout)
            assert report["applyOutcome"] == "committed"
            assert target.read_bytes() == original.replace("sample:lib:1.0.0", "sample:lib:1.1.0").encode()
            print("Wheel installation, both aliases, fixed metadata query/update and CRLF: PASS")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("wheel_dir", type=Path)
    smoke(parser.parse_args().wheel_dir)
