"""Render timestamped real harness output alongside its Playwright video.

Usage: python3 tools/render_demo.py /path/to/capture.json
Requires Pillow, ffmpeg and ffprobe. Capture must contain events, elapsed,
verify_exit, verify_output, prompt and root/video/*.webm. Diagnostic wrapper
logs are omitted; harness tool output and final text are preserved.
"""

import argparse
import json
import math
from pathlib import Path
import subprocess
import sys
import textwrap
from PIL import Image, ImageDraw, ImageFont

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("capture", type=Path)
parser.add_argument(
    "--diagnostic",
    action="store_true",
    help="Publish a clearly labelled failed run as diagnostic evidence",
)
parser.add_argument("--output", type=Path, default=Path("assets/demo.mp4"))
options = parser.parse_args()
capture = json.loads(options.capture.read_text())
provider = {
    "deepseek": "DeepSeek",
    "gemini": "Gemini",
    "google-ai-mode": "Google AI Mode",
}.get(capture.get("provider", "deepseek"), "Browser")
harness = capture.get("harness", "codewhale")
failed = bool(capture["exit"] or capture["verify_exit"])
if failed and not options.diagnostic:
    raise SystemExit("Refusing to publish a failed demo")
videos = [
    p
    for p in (Path(capture["root"]) / "video").iterdir()
    if p.suffix in {".webm", ".mp4"}
]
video = max(videos, key=lambda p: p.stat().st_size)
probe = json.loads(
    subprocess.check_output(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
            str(video),
        ]
    )
)
video_duration = float(probe["format"]["duration"])
elapsed = capture["elapsed"]
metadata = Path(str(video) + ".json")
offset = (
    max(0, json.loads(metadata.read_text())["start"] / 1000 - capture["started_at"])
    if metadata.exists()
    else max(0, elapsed - video_duration)
)
fps = 12
seconds = elapsed + 4
font_path = "/usr/share/fonts/google-noto-vf/NotoSansMono[wght].ttf"
font = ImageFont.truetype(font_path, 18)
heading = ImageFont.truetype(font_path, 24)
output = options.output
output.parent.mkdir(parents=True, exist_ok=True)
terminal = Path(capture["root"]) / "terminal.mp4"
encoder = subprocess.Popen(
    [
        "ffmpeg",
        "-y",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-s",
        "960x864",
        "-r",
        str(fps),
        "-i",
        "-",
        "-an",
        "-c:v",
        "libx264",
        "-preset",
        "fast",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        str(terminal),
    ],
    stdin=subprocess.PIPE,
)
response_demo = capture.get("kind") == "response"
if response_demo:
    initial = "$ " + harness + "  # actual harness run\n" + capture["prompt"] + "\n\n"
else:
    baseline = capture.get("baseline_output", capture["events"][1]["text"])
    summary = baseline[baseline.rfind("Ran ") :] if "Ran " in baseline else baseline
    initial = (
        "$ python3 -m unittest -v  # before the fix\n"
        + baseline.split("====")[0]
        + summary
        + "\n$ "
        + harness
        + "  # actual harness run\n"
        + capture["prompt"]
        + "\n\n"
    )
events = []
if harness == "opencode":
    pending = ""
    for event in capture["events"]:
        if event["channel"] != "stdout":
            continue
        pending += event["text"]
        while "\n" in pending:
            line, pending = pending.split("\n", 1)
            try:
                item = json.loads(line)
            except ValueError:
                continue
            part = item.get("part", {})
            if (
                item.get("type") == "tool_use"
                and part.get("state", {}).get("status") == "completed"
            ):
                state = part["state"]
                text = (
                    "tool "
                    + part.get("tool", "")
                    + " "
                    + json.dumps(state.get("input", {}), ensure_ascii=False)
                    + "\n"
                    + state.get("output", "")
                    + "\n"
                )
            elif item.get("type") == "text":
                text = part.get("text", "") + "\n"
            else:
                continue
            events.append(dict(event, text=text))
elif harness == "openclaw":
    raw = "".join(e["text"] for e in capture["events"] if e["channel"] == "stdout")
    try:
        result = json.loads(raw[raw.index("{") :])
        text = json.dumps(result, indent=2, ensure_ascii=False)
    except (ValueError, KeyError):
        text = raw
    events.append({"t": elapsed, "text": text})
else:
    events = [
        e
        for e in capture["events"]
        if (e["channel"] == "stdout" and e["t"] > 2)
        or (e["channel"] == "stderr" and e["text"].startswith("tool"))
    ]
for frame in range(math.ceil(seconds * fps)):
    t = frame / fps
    content = initial + "".join(e["text"] for e in events if e["t"] <= t)
    if t >= elapsed:
        content += (
            "\n\n$ independent response check\n"
            if response_demo
            else "\n\n$ python3 -m unittest -v  # independent verification\n"
        ) + capture["verify_output"]
    lines = []
    for line in content.splitlines():
        lines.extend(
            textwrap.wrap(
                line, width=82, replace_whitespace=False, drop_whitespace=False
            )
            or [""]
        )
    image = Image.new("RGB", (960, 864), "#111827")
    draw = ImageDraw.Draw(image)
    draw.rectangle((0, 0, 960, 64), fill="#202d43")
    draw.text(
        (24, 17),
        harness.upper()
        + (
            "  /  FAILED CODING RUN"
            if failed
            else (
                "  /  REAL HARNESS RESPONSE"
                if response_demo
                else "  /  REAL TOOL EXECUTION"
            )
        ),
        font=heading,
        fill="#e8efff",
    )
    for row, line in enumerate(lines[-31:]):
        color = (
            "#83e3a0" if line.startswith(("tool", "OK", "Fixed", "$")) else "#e0e7f1"
        )
        draw.text((20, 80 + row * 24), line, font=font, fill=color)
    draw.text(
        (20, 831), f"{t:04.1f}s  |  {provider} browser relay", font=font, fill="#99abc4"
    )
    encoder.stdin.write(image.tobytes())
encoder.stdin.close()
if encoder.wait():
    raise SystemExit("Terminal encoding failed")
filter_graph = f"[1:v]setpts=PTS-STARTPTS,tpad=start_duration={offset}:stop_mode=clone:stop_duration=5,pad=960:864:0:64:color=0x202d43,drawtext=fontfile='{font_path}':text='{provider.upper()}  /  LIVE BROWSER PAGE':x=24:y=17:fontsize=24:fontcolor=white[b];[0:v][b]hstack=inputs=2[v]"
subprocess.run(
    [
        "ffmpeg",
        "-y",
        "-loglevel",
        "error",
        "-i",
        str(terminal),
        "-i",
        str(video),
        "-filter_complex",
        filter_graph,
        "-map",
        "[v]",
        "-t",
        str(seconds),
        "-r",
        str(fps),
        "-an",
        "-c:v",
        "libx264",
        "-preset",
        "fast",
        "-crf",
        "21",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
        str(output),
    ],
    check=True,
)
subprocess.run(
    [
        "ffmpeg",
        "-y",
        "-loglevel",
        "error",
        "-i",
        str(output),
        "-filter_complex",
        "fps=8,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse",
        str(output.with_suffix(".gif")),
    ],
    check=True,
)
print(output)
