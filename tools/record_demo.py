"""Capture a real coding run and independently verify its tests.
Uses a copy of the provider profile and an isolated harness workspace.
Requires a built debug binary and the selected harness on PATH.
"""

import os, sys, tempfile, pathlib, shutil, subprocess, time, selectors, json, argparse, signal

parser = argparse.ArgumentParser()
parser.add_argument(
    "--provider", default="deepseek", choices=["deepseek", "gemini", "google-ai-mode"]
)
parser.add_argument(
    "--harness", default="opencode", choices=["codewhale", "opencode", "openclaw"]
)
parser.add_argument(
    "--scenario",
    default="invoice",
    choices=["invoice", "slug", "delegated", "response"],
)
parser.add_argument("--cdp-endpoint")
parser.add_argument(
    "--model", help="Select a configured page-state model, e.g. deepseek-pro"
)
parser.add_argument(
    "--compact-agent",
    action="store_true",
    help="Use a project-owned OpenCode agent with only bash; records this configuration in capture.json",
)
parser.add_argument(
    "--minimal-tool",
    action="store_true",
    help="Use the documented project tool override to shorten the bash schema; requires --compact-agent",
)
options = parser.parse_args()
if options.minimal_tool and not options.compact_agent:
    parser.error("--minimal-tool requires --compact-agent")
repo = pathlib.Path(__file__).resolve().parents[1]
root = pathlib.Path(tempfile.mkdtemp(prefix="freechatcode-demo-"))
(root / "home/freechatcode").mkdir(parents=True)
config = 'mode = "silent"\n[transport]\nmode = "gui"\n'
if options.provider != "deepseek":
    config += (repo / "assets/providers.catalog.toml").read_text()
(root / "home/freechatcode/config.toml").write_text(config)
profile = root / "profile"
(
    shutil.copytree(
        pathlib.Path.home() / ".codewhale/providers" / options.provider / "browser",
        profile,
        symlinks=True,
    )
    if not options.cdp_endpoint
    else profile.mkdir()
)
for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"]:
    (profile / name).unlink(missing_ok=True)
workspace = root / "workspace"
workspace.mkdir()
(workspace / "invoice.py").write_text(
    "def total(price, quantity):\n    return price + quantity\n"
)
(workspace / "test_invoice.py").write_text(
    'import unittest\nfrom invoice import total\n\nclass InvoiceTests(unittest.TestCase):\n    def test_three_items(self):\n        self.assertEqual(total(10, 3), 30)\n    def test_zero_items(self):\n        self.assertEqual(total(10, 0), 0)\n\nif __name__ == "__main__":\n    unittest.main()\n'
)
prompt = "Fix the incorrect invoice total in invoice.py. Read invoice.py and test_invoice.py using tools, edit the calculation, then run python3 -m unittest -v using the bash tool. Finish only after both tests pass, and briefly report the change."
if options.scenario == "slug":
    (workspace / "invoice.py").unlink()
    (workspace / "test_invoice.py").unlink()
    (workspace / "slug.py").write_text(
        'def slug(text):\n    return text.lower().replace(" ", "-")\n'
    )
    (workspace / "test_slug.py").write_text(
        'import unittest\nfrom slug import slug\nclass SlugTests(unittest.TestCase):\n    def test_trim(self): self.assertEqual(slug("  Hello World  "),"hello-world")\n    def test_repeated_spaces(self): self.assertEqual(slug("hello   world"),"hello-world")\n    def test_tabs(self): self.assertEqual(slug("hello\tworld"),"hello-world")\n    def test_empty(self): self.assertEqual(slug("   "),"")\n'
    )
    prompt = "Fix slug.py so slug normalizes all whitespace and removes leading/trailing whitespace. Read the source and tests using tools, edit the implementation, then run python3 -m unittest -v. Finish only after all tests pass. Briefly report the change."
expected_response = options.provider.upper().replace("-", "_") + "_HARNESS_CHECK"
if options.scenario == "response":
    if options.harness != "opencode" or not options.compact_agent:
        parser.error("response scenario requires OpenCode and --compact-agent")
    (workspace / "invoice.py").unlink()
    (workspace / "test_invoice.py").unlink()
    prompt = f"Reply exactly {expected_response} and nothing else."
if options.scenario == "delegated":
    if options.harness != "opencode":
        parser.error("delegated scenario requires OpenCode")
    prompt = "Use the task tool to delegate reading invoice.py and test_invoice.py to an explore subagent. After its report, fix the calculation yourself and run python3 -m unittest -v. Finish only after both tests pass."
if options.compact_agent:
    if options.harness != "opencode":
        parser.error("--compact-agent requires --harness opencode")
    agent = {
        "description": "Fix and test the isolated Python fixture",
        "mode": "primary",
        "prompt": "You are a coding agent with a real bash tool supplied by the harness. Read the fixture files, fix the code, and run its tests. When you need a tool, return JSON with type tool_calls and tool_calls containing function name bash and arguments as an object with command and description. The harness executes your call and returns the result. Do not claim tools are unavailable. After tests pass, report the change briefly.",
        "permission": {"*": "deny", "bash": "allow"},
    }
    if options.scenario == "response":
        agent["prompt"] = "Answer the user's request directly and literally."
        agent["permission"] = {"*": "deny"}
    agents = {"fixture": agent}
    if options.scenario == "delegated":
        agent["permission"]["task"] = "allow"
        agent["prompt"] = (
            "You are a coding agent with real task and bash tools. Delegate the requested file inspection to explore, then fix and test the fixture. To run a tool, return a JSON object with type tool_calls and a tool_calls array containing function name and arguments object. The harness executes the tool and returns the result. After tests pass, report the change."
        )
        agents["explore"] = {
            "description": "Read the requested fixture source and tests",
            "mode": "subagent",
            "prompt": "You have a real read tool executed by the harness. Read the two requested files. To call read, return JSON with type tool_calls and a tool_calls array containing function name read and arguments object with filePath. After both tool results arrive, return their contents to the parent. A promise to read is not a tool call.",
            "permission": {"*": "deny", "read": "allow"},
        }
    (workspace / "opencode.json").write_text(json.dumps({"agent": agents}))
if options.minimal_tool:
    tools_dir = workspace / ".opencode/tools"
    tools_dir.mkdir(parents=True)
    shutil.copyfile(repo / "tools/fixtures/opencode-bash.ts", tools_dir / "bash.ts")
args = [
    str(repo / "target/debug/freechatcode"),
    "--mode",
    "silent",
    "--harness",
    options.harness,
    "--harness-bin",
    shutil.which(options.harness) or parser.error(f"{options.harness} is not on PATH"),
    "--chatmodel",
    options.provider,
    "--profile-dir",
    str(profile),
    "--record-video",
    str(root / "video"),
    "--record-video-size",
    "960x800",
]
if options.model:
    args += ["--model", options.model]
if options.cdp_endpoint:
    args += ["--cdp-endpoint", options.cdp_endpoint]
if options.harness == "codewhale":
    args += ["--", "exec", "--auto", prompt]
elif options.harness == "opencode":
    args += (
        ["--", "run", "--format", "json"]
        + (["--agent", "fixture"] if options.compact_agent else [])
        + [prompt]
    )
else:
    args += ["--", "--json", prompt]

recorder = None
if options.cdp_endpoint:
    driver = max(
        (pathlib.Path.home() / ".cache/playwright-rust").glob("*/playwright-*-linux"),
        key=lambda p: p.stat().st_mtime,
    )
    recorder = subprocess.Popen(
        [
            str(driver / "node"),
            str(repo / "tools/capture_browser.cjs"),
            options.cdp_endpoint,
            str(root / "video"),
            "gemini.google.com" if options.provider == "gemini" else "google.com",
            str(workspace),
        ],
        env=dict(os.environ, FREECHATCODE_PLAYWRIGHT_PACKAGE=str(driver / "package")),
    )
    ready_by = time.monotonic() + 20
    while not (root / "video/ready").exists():
        if recorder.poll() is not None or time.monotonic() > ready_by:
            raise RuntimeError("browser recorder did not start")
        time.sleep(0.1)
started_at = time.time()
start = time.monotonic()
events = [{"t": 0, "channel": "task", "text": prompt}]
initial = None
if options.scenario != "response":
    initial = subprocess.run(
        [sys.executable, "-m", "unittest", "-v"],
        cwd=workspace,
        capture_output=True,
        text=True,
    )
    events.append({"t": 0, "channel": "baseline", "text": initial.stderr})
process = subprocess.Popen(
    args,
    cwd=workspace,
    env=dict(os.environ, CODEWHALE_HOME=str(root / "home"), PWD=str(workspace)),
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    start_new_session=True,
)
sel = selectors.DefaultSelector()
for channel, pipe in [("stdout", process.stdout), ("stderr", process.stderr)]:
    sel.register(pipe, selectors.EVENT_READ, channel)
while sel.get_map():
    if time.monotonic() - start > 300:
        os.killpg(process.pid, signal.SIGTERM)
        events.append(
            {
                "t": time.monotonic() - start,
                "channel": "error",
                "text": "Demo exceeded 300 seconds; run terminated.",
            }
        )
        break
    for key, _ in sel.select(timeout=0.3):
        chunk = os.read(key.fileobj.fileno(), 65536)
        if not chunk:
            sel.unregister(key.fileobj)
            continue
        event = {
            "t": time.monotonic() - start,
            "channel": key.data,
            "text": chunk.decode("utf8", "replace"),
        }
        events.append(event)
        print(event["text"], end="", flush=True)
try:
    code = process.wait(timeout=10)
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGKILL)
    code = process.wait()
elapsed = time.monotonic() - start
if recorder:
    (root / "video/stop").touch()
    try:
        recorder.wait(timeout=30)
    except subprocess.TimeoutExpired:
        recorder.terminate()
        recorder.wait(timeout=10)
if options.scenario == "response":
    output = "".join(event["text"] for event in events if event["channel"] == "stdout")
    actual = ""
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get("type") == "text":
            actual += item.get("part", {}).get("text", "")
    verified = code == 0 and actual.strip() == expected_response
    verification = subprocess.CompletedProcess(
        [],
        0 if verified else 1,
        f"Expected: {expected_response}\nActual: {actual.strip()}\n{'PASS' if verified else 'FAIL'}\n",
        "",
    )
else:
    verification = subprocess.run(
        [sys.executable, "-m", "unittest", "-v"],
        cwd=workspace,
        capture_output=True,
        text=True,
    )
result = {
    "root": str(root),
    "elapsed": elapsed,
    "exit": code,
    "verify_exit": verification.returncode,
    "verify_output": verification.stdout + verification.stderr,
    "events": events,
    "prompt": prompt,
    "harness": options.harness,
    "provider": options.provider,
    "scenario": options.scenario,
    "command": args,
    "baseline_output": initial.stdout + initial.stderr if initial else None,
    "baseline_exit": initial.returncode if initial else None,
    "kind": "response" if options.scenario == "response" else "coding",
    "started_at": started_at,
    "compact_agent": options.compact_agent,
    "minimal_tool": options.minimal_tool,
    "model": options.model,
}
(root / "capture.json").write_text(json.dumps(result, indent=2))
pathlib.Path(
    "/tmp/freechatcode-demo-"
    + options.provider
    + "-"
    + options.harness
    + "-"
    + options.scenario
    + "-path"
).write_text(str(root))
print(f"\nCAPTURE {root / 'capture.json'}", flush=True)
print(
    "\nDEMO_RESULT",
    json.dumps({k: v for k, v in result.items() if k not in ["events", "prompt"]}),
)
if (
    code != 0
    or verification.returncode != 0
    or (initial is not None and initial.returncode == 0)
):
    sys.exit(1)
