#!/usr/bin/env python3
"""Extract text from an image using a local OCR backend.

Backends:
    tesseract   the local `tesseract` binary (fast, preferred when installed)
    ollama      a local Ollama vision model (see tools/vision.py)
    auto        tesseract when installed, otherwise ollama (default)

Only extracted text is written to stdout; diagnostics go to stderr.

Examples:
    python3 tools/ocr.py screenshot.png
    python3 tools/ocr.py screenshot.png --backend ollama --model gemma4:26b
    python3 tools/ocr.py screenshot.png --backend tesseract --lang eng --psm 6
    python3 tools/ocr.py --list-backends

Exit codes:
    0  success (an empty result is still success)
    1  usage/other error
    2  Ollama server unreachable
    3  Ollama server reachable but the vision model is missing
    4  the selected backend is unavailable or failed
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import subprocess
import sys

OCR_PROMPT = (
    "You are an OCR engine. Transcribe every piece of visible text in this "
    "image exactly as it appears, preserving line breaks and reading order. "
    "Output only the transcribed text: no commentary, no headings, no quotes "
    "and no code fences. If the image contains no text, output nothing."
)

DEFAULT_TIMEOUT = 300.0

EXIT_OK = 0
EXIT_ERROR = 1
EXIT_UNREACHABLE = 2
EXIT_MODEL_UNAVAILABLE = 3
EXIT_BACKEND_FAILED = 4

# Tesseract is often installed outside PATH (e.g. Linuxbrew without shell init).
TESSERACT_CANDIDATES = (
    os.environ.get("TESSERACT_BIN"),
    "/home/linuxbrew/.linuxbrew/bin/tesseract",
    "/opt/homebrew/bin/tesseract",
    "/usr/local/bin/tesseract",
    "/usr/bin/tesseract",
)


class OcrError(Exception):
    """Fatal, user-facing problem. `code` becomes the process exit status."""

    def __init__(self, message: str, code: int = EXIT_ERROR):
        super().__init__(message)
        self.code = code


def warn(message: str) -> None:
    print(f"ocr.py: {message}", file=sys.stderr)


# --------------------------------------------------------------------------- #
# backends
# --------------------------------------------------------------------------- #
def find_tesseract() -> str | None:
    """Absolute path to the tesseract binary, or None when not installed."""
    for candidate in TESSERACT_CANDIDATES:
        if candidate and os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    import shutil

    return shutil.which("tesseract")


def _load_vision_module():
    """Import the sibling tools/vision.py without touching sys.path."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vision.py")
    if not os.path.isfile(path):
        return None
    spec = importlib.util.spec_from_file_location("_deepchat_vision", path)
    if spec is None or spec.loader is None:  # pragma: no cover
        return None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def strip_code_fences(text: str) -> str:
    """Drop a single ```/```text wrapper some models add around OCR output."""
    stripped = text.strip()
    if not stripped.startswith("```"):
        return stripped
    lines = stripped.splitlines()
    if lines and lines[0].lstrip().startswith("```"):
        lines = lines[1:]
    if lines and lines[-1].strip() == "```":
        lines = lines[:-1]
    return "\n".join(lines).strip()


def run_tesseract(
    image: str,
    binary: str,
    lang: str | None = None,
    psm: int | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> str:
    cmd = [binary, image, "stdout"]
    if lang:
        cmd += ["-l", lang]
    if psm is not None:
        cmd += ["--psm", str(psm)]
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired as exc:
        raise OcrError(
            f"tesseract timed out after {timeout:g}s", EXIT_BACKEND_FAILED
        ) from exc
    except OSError as exc:
        raise OcrError(f"could not run {binary}: {exc}", EXIT_BACKEND_FAILED) from exc

    if proc.returncode != 0:
        detail = (proc.stderr or "").strip().splitlines()
        detail_text = detail[-1] if detail else "no stderr"
        raise OcrError(
            f"tesseract failed (exit {proc.returncode}): {detail_text[:300]}",
            EXIT_BACKEND_FAILED,
        )
    return proc.stdout


def run_ollama(
    image: str,
    prompt: str = OCR_PROMPT,
    model: str | None = None,
    host: str | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> str:
    vision = _load_vision_module()
    if vision is None:
        raise OcrError(
            "tools/vision.py was not found next to ocr.py, so the ollama "
            "backend is unavailable",
            EXIT_BACKEND_FAILED,
        )
    try:
        text = vision.generate(
            [image],
            prompt=prompt,
            model=model,
            host=host,
            timeout=timeout,
            options={"temperature": 0},
        )
    except vision.OllamaError as exc:
        raise OcrError(str(exc), getattr(exc, "code", EXIT_ERROR)) from exc
    return strip_code_fences(text)


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #
def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="ocr.py",
        description="Extract text from an image with a local OCR backend.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Env: OLLAMA_VISION_MODEL / VISION_MODEL (model), OLLAMA_HOST (server).\n"
            "Examples:\n"
            "  python3 tools/ocr.py shot.png\n"
            "  python3 tools/ocr.py shot.png --backend ollama\n"
            "  python3 tools/ocr.py --list-backends\n"
        ),
    )
    parser.add_argument("image", nargs="?", help="path to the image file")
    parser.add_argument(
        "--backend",
        choices=("auto", "tesseract", "ollama"),
        default="auto",
        help="OCR backend (default: auto)",
    )
    parser.add_argument(
        "--model",
        default=None,
        help="ollama vision model (default: $OLLAMA_VISION_MODEL or gemma4:26b)",
    )
    parser.add_argument(
        "--prompt",
        default=OCR_PROMPT,
        help="prompt used by the ollama backend",
    )
    parser.add_argument("--host", default=None, help="Ollama base URL")
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT,
        help=f"per-backend timeout in seconds (default: {DEFAULT_TIMEOUT:g})",
    )
    parser.add_argument("--lang", default=None, help="tesseract language, e.g. eng")
    parser.add_argument(
        "--psm", type=int, default=None, help="tesseract page segmentation mode (0-13)"
    )
    parser.add_argument(
        "--list-backends",
        action="store_true",
        help="report which backends are available, then exit",
    )
    return parser


def list_backends(args) -> int:
    tess = find_tesseract()
    print(f"tesseract: {tess}" if tess else "tesseract: not found")

    vision = _load_vision_module()
    if vision is None:
        print("ollama:    unavailable (tools/vision.py missing)")
        return EXIT_OK
    try:
        info = vision.check(args.model, args.host, args.timeout)
    except vision.OllamaError as exc:
        first_line = str(exc).splitlines()[0]
        print(f"ollama:    unavailable ({first_line})")
        return EXIT_OK
    print(f"ollama:    available at {info['host']} (model {info['model']})")
    return EXIT_OK


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    try:
        if args.list_backends:
            return list_backends(args)

        if not args.image:
            parser.error("an image path is required (or use --list-backends)")
        if not os.path.isfile(args.image):
            raise OcrError(f"image not found: {args.image}")

        tess = find_tesseract()
        backend = args.backend
        if backend == "auto":
            backend = "tesseract" if tess else "ollama"
            if backend == "ollama":
                warn("tesseract not installed; using the ollama backend")

        if backend == "tesseract" and tess is None:
            raise OcrError(
                "tesseract is not installed (looked in PATH and the usual "
                "brew prefixes).\n"
                "  Install it with:  brew install tesseract\n"
                "  Or use the vision model:  --backend ollama",
                EXIT_BACKEND_FAILED,
            )

        text = ""
        use_ollama = backend == "ollama"
        if backend == "tesseract":
            try:
                text = run_tesseract(
                    args.image, tess, args.lang, args.psm, args.timeout
                )
            except OcrError as exc:
                if args.backend != "auto":
                    raise
                warn(f"{exc}; falling back to ollama")
                use_ollama = True
            else:
                if args.backend == "auto" and not text.strip():
                    warn("tesseract extracted no text; retrying via ollama")
                    use_ollama = True

        if use_ollama:
            text = run_ollama(
                args.image, args.prompt, args.model, args.host, args.timeout
            )

        if text:
            sys.stdout.write(text if text.endswith("\n") else text + "\n")
        return EXIT_OK
    except OcrError as exc:
        warn(str(exc))
        return exc.code
    except KeyboardInterrupt:  # pragma: no cover
        return 130


if __name__ == "__main__":
    sys.exit(main())
