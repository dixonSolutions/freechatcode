# Local vision + OCR tools

Two small Python 3 CLIs for analysing images **entirely on this machine** — no
cloud API, no API keys.

| Script | Purpose | Backend |
| --- | --- | --- |
| `tools/vision.py` | Describe an image in natural language | local Ollama vision model |
| `tools/ocr.py` | Extract the text in an image | `tesseract` if installed, else the local Ollama vision model |

Both scripts use only the Python standard library (`requests` is used
automatically when it happens to be installed). No numpy/OpenCV/PyTorch needed.

## Requirements

- A running Ollama server (`ollama serve`, default `http://127.0.0.1:11434`).
- A vision-capable model, e.g. `gemma4:26b` (already pulled on this machine).
- Optional: the `tesseract` binary for fast, deterministic OCR.

Check what is available right now:

```bash
python3 tools/ocr.py --list-backends
python3 tools/vision.py --check
```

`--list-backends` prints the tesseract path (or `tesseract: not found`) and
whether the Ollama server and vision model are reachable.

## vision.py — describe an image

```bash
python3 tools/vision.py screenshot.png
python3 tools/vision.py screenshot.png --prompt "What error is shown in this dialog?"
python3 tools/vision.py screenshot.png --model gemma4:26b --host http://127.0.0.1:11434
python3 tools/vision.py --check            # server + model reachable?
python3 tools/vision.py --list-models      # models installed on the server
```

Flags:

- `--model` — vision model (default `$OLLAMA_VISION_MODEL`, else `gemma4:26b`).
- `--prompt` — prompt sent with the image.
- `--host` — Ollama base URL (default `$OLLAMA_HOST`, else `http://127.0.0.1:11434`).
- `--timeout` — per-request timeout in seconds (default `300`; a 26B model can
  take a minute or more on first load).

Environment: `OLLAMA_VISION_MODEL` (or `VISION_MODEL`), `OLLAMA_HOST`.

Exit codes: `0` success · `1` bad path/other error · `2` Ollama unreachable ·
`3` reachable but the model is not installed.

## ocr.py — extract text

```bash
python3 tools/ocr.py screenshot.png                          # auto
python3 tools/ocr.py screenshot.png --backend tesseract --lang eng --psm 6
python3 tools/ocr.py screenshot.png --backend ollama --timeout 600
python3 tools/ocr.py --list-backends
```

- `--backend auto` (default): uses `tesseract` when it is installed; if
  tesseract fails or returns nothing, it falls back to the Ollama vision model
  and says so on stderr.
- `--backend tesseract`: fails with a clear message (exit `4`) if tesseract is
  missing, instead of silently falling back.
- `--backend ollama`: always uses the vision model with a strict
  "transcribe only, no commentary" prompt; ```` ``` ```` fences are stripped.

**Only the extracted text is written to stdout**, so it pipes cleanly:

```bash
python3 tools/ocr.py shot.png | grep -i total
```

Diagnostics (which backend ran, fallbacks, errors) go to stderr. An empty
result is still exit code `0`.

Exit codes: `0` success · `1` bad path/other error · `2` Ollama unreachable ·
`3` vision model missing · `4` selected backend unavailable or failed.

`ocr.py` finds tesseract on `PATH` and also at the usual Homebrew/Linuxbrew
prefixes (`/home/linuxbrew/.linuxbrew/bin`, `/opt/homebrew/bin`,
`/usr/local/bin`, `/usr/bin`), so it works even when brew's shell init is not
loaded. Override with `TESSERACT_BIN=/path/to/tesseract`.

## Installing tesseract (optional)

No sudo is required on this machine — Linuxbrew installs under `/home/linuxbrew`:

```bash
HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 \
  /home/linuxbrew/.linuxbrew/bin/brew install tesseract
```

Status on this machine: **installed and verified** — `tesseract 5.5.3`
(leptonica-1.87.0) at `/home/linuxbrew/.linuxbrew/bin/tesseract`, install took
about 5 minutes and poured an x86_64_linux bottle for `tesseract` plus its
dependencies (`libb2`, `libarchive`, `fribidi`, `libdatrie`, `libthai`,
`pango`). Language data is exactly what the `tesseract` formula ships:
**`eng` and `osd`**. For more languages, `brew install tesseract-lang`.

`ocr.py` finds that binary even when brew's shell init is not loaded, so no
`PATH` changes are required. The Ollama backend needs no extra installation at
all, so OCR keeps working on machines without tesseract.

## Verification

Reproduce the checks for this setup:

```bash
# 1. both scripts parse and expose help
python3 -m py_compile tools/vision.py tools/ocr.py
python3 tools/vision.py --help >/dev/null && python3 tools/ocr.py --help >/dev/null

# 2. which backends exist right now
python3 tools/ocr.py --list-backends

# 3. OCR against an image with known text
#    Render at a decent size (>=28px): tiny 10px default-font text is where
#    OCR, especially the vision model, starts misreading characters.
python3 - <<'PY'
import glob
from PIL import Image, ImageDraw, ImageFont
lines = ["DeepChatCode OCR self-test", "Invoice 2024-0042",
         "Total: 137.50 USD", "handshake-verified"]
cands = glob.glob('/usr/share/fonts/**/*.ttf', recursive=True)
font = ImageFont.truetype(cands[0], 30) if cands else ImageFont.load_default()
img = Image.new("RGB", (820, 240), "white")
draw = ImageDraw.Draw(img)
for i, line in enumerate(lines):
    draw.text((24, 24 + i * 52), line, fill="black", font=font)
img.save("/tmp/ocr_probe.png")
print("\n".join(lines))
PY
python3 tools/ocr.py /tmp/ocr_probe.png                              # auto: tesseract
python3 tools/ocr.py /tmp/ocr_probe.png --backend tesseract --psm 6
python3 tools/ocr.py /tmp/ocr_probe.png --backend ollama             # vision-model path
# each of the three must print the same 4 lines; text only on stdout

# 4. describe a real image
python3 tools/vision.py /tmp/ocr_probe.png --prompt "Describe this image"

# 5. error handling is non-zero and explicit
python3 tools/vision.py /tmp/missing.png ; echo "exit=$?"                     # 1
python3 tools/vision.py --model no-such-model:1b /tmp/ocr_probe.png; echo "exit=$?"  # 3
python3 tools/vision.py --host http://127.0.0.1:59999 /tmp/ocr_probe.png; echo "exit=$?"  # 2
```

### Results observed on this machine

Run 2026-10-08 with tesseract 5.5.3 (Linuxbrew) and `gemma4:26b` (Ollama):

- `python3 tools/ocr.py --list-backends` → `tesseract: /home/linuxbrew/.linuxbrew/bin/tesseract`
  and `ollama: available at http://127.0.0.1:11434 (model gemma4:26b)`.
- OCR of the synthetic known-text image (30px font), for `auto` (tesseract),
  `--backend tesseract --psm 6` and `--backend ollama`: **all four lines
  returned exactly**, exit code 0, and in `auto` mode stderr was empty
  (diagnostics-only contract holds).
- Vision: `python3 tools/vision.py <screenshot>` correctly described an
  X11/Chromium screenshot, including the on-screen clock text and the page URL;
  a cold 26B run took ~3m50s, later calls with the model warm are much faster.
- Error paths returned the documented codes: missing image → `1`, unknown
  model → `3`, unreachable server → `2`.
- Caveat seen in practice: with tiny (~10px) default-font text, the vision
  model misread characters (`Total` → `Totat`, `137` → `187`). Tesseract read
  the same image better. Render or capture text legibly, or verify OCR output
  on a real sample before trusting it.

## Notes / limits

- Images are sent to Ollama base64-encoded as-is; very large screenshots make
  requests slow. Downscale first if a call feels sluggish.
- Vision-model OCR can hallucinate on low-contrast or heavily styled text;
  prefer tesseract for anything that must be exact.
- Both scripts talk to `127.0.0.1` by default, so no image ever leaves the box.
