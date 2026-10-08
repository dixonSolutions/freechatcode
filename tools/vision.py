#!/usr/bin/env python3
"""Describe an image with a local Ollama vision model.

Examples:
    python3 tools/vision.py screenshot.png
    python3 tools/vision.py screenshot.png --prompt "What text is on screen?"
    python3 tools/vision.py --check          # is the server/model usable?

Talks HTTP to a local Ollama server (default http://127.0.0.1:11434) and uses a
vision-capable model (default gemma4:26b).  Only the Python standard library is
required; `requests` is used when it happens to be installed.

Exit codes:
    0  success
    1  usage/other error (bad path, malformed response, ...)
    2  Ollama server unreachable
    3  server reachable but the requested model is not available
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import sys

DEFAULT_MODEL = "gemma4:26b"
DEFAULT_HOST = "http://127.0.0.1:11434"
DEFAULT_PROMPT = (
    "Describe this image in detail: the main subjects, any visible text, "
    "colors, layout and anything else notable."
)
DEFAULT_TIMEOUT = 300.0

EXIT_OK = 0
EXIT_ERROR = 1
EXIT_UNREACHABLE = 2
EXIT_MODEL_UNAVAILABLE = 3


class OllamaError(Exception):
    """Fatal, user-facing problem. `code` becomes the process exit status."""

    def __init__(self, message: str, code: int = EXIT_ERROR):
        super().__init__(message)
        self.code = code


# --------------------------------------------------------------------------- #
# configuration helpers
# --------------------------------------------------------------------------- #
def normalize_host(host: str | None = None) -> str:
    """Return a scheme-qualified, slash-trimmed Ollama base URL."""
    value = (host or os.environ.get("OLLAMA_HOST") or DEFAULT_HOST).strip()
    if not value:
        value = DEFAULT_HOST
    if not value.startswith(("http://", "https://")):
        value = "http://" + value
    return value.rstrip("/")


def resolve_model(model: str | None = None) -> str:
    """--model > $OLLAMA_VISION_MODEL > $VISION_MODEL > built-in default."""
    chosen = (
        model
        or os.environ.get("OLLAMA_VISION_MODEL")
        or os.environ.get("VISION_MODEL")
        or DEFAULT_MODEL
    )
    return chosen.strip()


def _model_available(wanted: str, names: list[str]) -> bool:
    want = wanted.strip().lower()
    if not want:
        return False
    if ":" in want:
        # Explicit tag: require an exact match.
        return any(n.strip().lower() == want for n in names)
    # No tag given: accept any tag of that model family.
    return any(n.strip().lower().split(":")[0] == want for n in names)


# --------------------------------------------------------------------------- #
# HTTP plumbing
# --------------------------------------------------------------------------- #
def _unreachable(url: str, exc: BaseException) -> "OllamaError":
    return OllamaError(
        f"cannot reach the Ollama server at {url} ({exc}).\n"
        "  Start it with:  ollama serve\n"
        "  Or point elsewhere with --host / $OLLAMA_HOST.",
        EXIT_UNREACHABLE,
    )


def _http_error_code(status: int, body: str) -> int:
    lowered = body.lower()
    if status == 404 or "not found" in lowered or "no such model" in lowered:
        return EXIT_MODEL_UNAVAILABLE
    if status in (500, 502, 503, 504):
        return EXIT_UNREACHABLE
    return EXIT_ERROR


def _http(method: str, url: str, payload=None, timeout: float = DEFAULT_TIMEOUT):
    """Return (status_code, body_text). Connection failures raise OllamaError."""
    body = None
    headers = {}
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"

    try:
        import requests  # optional: use it when present
    except Exception:  # pragma: no cover - depends on environment
        requests = None

    if requests is not None:
        try:
            resp = requests.request(
                method, url, data=body, headers=headers, timeout=timeout
            )
        except OSError as exc:  # RequestException subclasses IOError
            raise _unreachable(url, exc) from exc
        return resp.status_code, resp.text

    import urllib.error
    import urllib.request

    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")
    except OSError as exc:
        raise _unreachable(url, exc) from exc


# --------------------------------------------------------------------------- #
# Ollama API
# --------------------------------------------------------------------------- #
def list_models(host: str | None = None, timeout: float = DEFAULT_TIMEOUT) -> list[str]:
    base = normalize_host(host)
    status, text = _http("GET", f"{base}/api/tags", timeout=timeout)
    if status != 200:
        raise OllamaError(
            f"Ollama server at {base} answered HTTP {status}: {text.strip()[:300]}",
            _http_error_code(status, text),
        )
    try:
        data = json.loads(text)
    except ValueError as exc:
        raise OllamaError(
            f"Ollama server at {base} returned a non-JSON body: {text.strip()[:300]}",
        ) from exc
    return [m.get("name", "") for m in data.get("models", []) if m.get("name")]


def ensure_model(
    host: str | None = None,
    model: str | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> list[str]:
    """Raise OllamaError unless `model` is installed. Returns all model names."""
    wanted = resolve_model(model)
    names = list_models(host, timeout)
    if _model_available(wanted, names):
        return names
    installed = ", ".join(sorted(names)) if names else "(none)"
    raise OllamaError(
        f"model {wanted!r} is not available on {normalize_host(host)}.\n"
        f"  Installed models: {installed}\n"
        f"  Pull it with:  ollama pull {wanted}",
        EXIT_MODEL_UNAVAILABLE,
    )


def encode_image(path: str) -> str:
    if not os.path.isfile(path):
        raise OllamaError(f"image not found: {path}")
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as exc:
        raise OllamaError(f"cannot read image {path}: {exc}") from exc
    if not data:
        raise OllamaError(f"image is empty: {path}")
    return base64.b64encode(data).decode("ascii")


def generate(
    image_paths,
    prompt: str = DEFAULT_PROMPT,
    model: str | None = None,
    host: str | None = None,
    timeout: float = DEFAULT_TIMEOUT,
    options=None,
) -> str:
    """Send image(s) + prompt to Ollama and return the model's text reply."""
    base = normalize_host(host)
    target = resolve_model(model)
    ensure_model(base, target, timeout)

    payload = {
        "model": target,
        "prompt": prompt,
        "images": [encode_image(p) for p in image_paths],
        "stream": False,
    }
    if options:
        payload["options"] = options

    status, text = _http("POST", f"{base}/api/generate", payload, timeout)
    if status != 200:
        raise OllamaError(
            f"Ollama /api/generate failed with HTTP {status}: {text.strip()[:300]}",
            _http_error_code(status, text),
        )
    try:
        data = json.loads(text)
    except ValueError as exc:
        raise OllamaError(
            f"Ollama returned a non-JSON reply: {text.strip()[:300]}"
        ) from exc
    error = data.get("error")
    if error:
        raise OllamaError(
            f"Ollama error: {error}", _http_error_code(status, str(error))
        )
    return (data.get("response") or "").strip()


def describe(
    image: str,
    prompt: str = DEFAULT_PROMPT,
    model: str | None = None,
    host: str | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> str:
    """Describe a single image file. Convenience wrapper around generate()."""
    return generate([image], prompt=prompt, model=model, host=host, timeout=timeout)


def check(
    model: str | None = None,
    host: str | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict:
    base = normalize_host(host)
    names = ensure_model(base, model, timeout)
    return {"host": base, "model": resolve_model(model), "models": names}


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #
def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="vision.py",
        description="Describe an image with a local Ollama vision model.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Env: OLLAMA_VISION_MODEL (model), OLLAMA_HOST (server URL).\n"
            "Examples:\n"
            "  python3 tools/vision.py shot.png\n"
            "  python3 tools/vision.py shot.png --prompt 'Read the text'\n"
            "  python3 tools/vision.py --check\n"
        ),
    )
    parser.add_argument("image", nargs="?", help="path to the image file")
    parser.add_argument(
        "--model",
        default=None,
        help=f"vision model name (default: $OLLAMA_VISION_MODEL or {DEFAULT_MODEL})",
    )
    parser.add_argument(
        "--prompt",
        default=DEFAULT_PROMPT,
        help="prompt sent along with the image",
    )
    parser.add_argument(
        "--host",
        default=None,
        help=f"Ollama base URL (default: $OLLAMA_HOST or {DEFAULT_HOST})",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT,
        help=f"request timeout in seconds (default: {DEFAULT_TIMEOUT:g})",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify the server is reachable and the model is installed, then exit",
    )
    parser.add_argument(
        "--list-models",
        action="store_true",
        help="list models installed on the Ollama server, then exit",
    )
    return parser


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    try:
        if args.list_models:
            for name in sorted(list_models(args.host, args.timeout)):
                print(name)
            return EXIT_OK

        if args.check:
            info = check(args.model, args.host, args.timeout)
            print(f"ollama:  {info['host']} reachable")
            print(f"model:   {info['model']} available")
            print(f"models:  {', '.join(sorted(info['models']))}")
            return EXIT_OK

        if not args.image:
            parser.error("an image path is required (or use --check)")

        text = describe(
            args.image,
            prompt=args.prompt,
            model=args.model,
            host=args.host,
            timeout=args.timeout,
        )
        if text:
            print(text)
        return EXIT_OK
    except OllamaError as exc:
        print(f"vision.py: {exc}", file=sys.stderr)
        return exc.code
    except KeyboardInterrupt:  # pragma: no cover
        return 130


if __name__ == "__main__":
    sys.exit(main())
