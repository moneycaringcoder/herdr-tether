#!/usr/bin/env python3
"""Validate public Markdown links and synchronized documentation contracts.

The checker intentionally uses only the Python standard library.  Diagnostics contain
only repository-relative public names, are sorted, and are capped so malformed input
cannot turn CI output into a path or data disclosure channel.
"""

from __future__ import annotations

import argparse
from collections import Counter
import html
import os
from pathlib import Path, PurePosixPath
import re
import stat
import sys
import tomllib
import unicodedata
from urllib.parse import unquote, urlsplit

MAX_DOCUMENTS = 512
MAX_DOCUMENT_BYTES = 2 * 1024 * 1024
MAX_DIAGNOSTICS = 100
SKIPPED_SCHEMES = {"http", "https", "mailto", "ftp", "data"}


class BoundedTextTooLarge(Exception):
    """Raised when a text input exceeds the checker byte limit."""


def contains_forbidden_control(value: str) -> bool:
    return any(unicodedata.category(character).startswith("C") for character in value)


class Findings:
    def __init__(self) -> None:
        self.items: set[str] = set()

    def add(self, category: str, public_file: str, message: str) -> None:
        # Callers supply fixed messages or already-redacted relative names only.
        self.items.add(f"{category} {public_file}: {message}")

    def output(self) -> list[str]:
        ordered = sorted(self.items)
        if len(ordered) <= MAX_DIAGNOSTICS:
            return ordered
        marker = (
            f"diagnostic-limit <root>: omitted "
            f"{len(ordered) - (MAX_DIAGNOSTICS - 1)} additional diagnostics"
        )
        return sorted(ordered[: MAX_DIAGNOSTICS - 1] + [marker])


def public_name(path: Path, root: Path) -> str:
    return path.relative_to(root).as_posix()


def read_bounded_utf8(path: Path) -> str:
    """Read strict UTF-8 from one regular-file descriptor, bounded at MAX + 1."""
    with path.open("rb") as document:
        metadata = os.fstat(document.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise OSError("not a regular file")
        if metadata.st_size > MAX_DOCUMENT_BYTES:
            raise BoundedTextTooLarge
        content = document.read(MAX_DOCUMENT_BYTES + 1)
    if len(content) > MAX_DOCUMENT_BYTES:
        raise BoundedTextTooLarge
    return content.decode("utf-8", errors="strict")


def read_public(path: Path, root: Path, findings: Findings) -> str | None:
    name = public_name(path, root)
    try:
        return read_bounded_utf8(path)
    except BoundedTextTooLarge:
        findings.add("document-size", name, f"exceeds {MAX_DOCUMENT_BYTES} bytes")
        return None
    except (OSError, UnicodeError):
        findings.add("document-read", name, "cannot read as bounded UTF-8 text")
        return None


def unfenced_lines(text: str):
    """Yield Markdown lines outside CommonMark-style backtick/tilde fences."""
    fence_char = ""
    fence_length = 0
    for line in text.splitlines():
        candidate = line.lstrip(" ") if len(line) - len(line.lstrip(" ")) <= 3 else line
        match = re.match(r"(`{3,}|~{3,})", candidate)
        if match:
            marker = match.group(1)
            if not fence_char:
                fence_char, fence_length = marker[0], len(marker)
                continue
            if marker[0] == fence_char and len(marker) >= fence_length and not candidate[len(marker):].strip():
                fence_char = ""
                fence_length = 0
                continue
        if not fence_char:
            yield line


def find_close(text: str, start: int, opening: str, closing: str) -> int | None:
    depth = 1
    escaped = False
    for index in range(start, len(text)):
        char = text[index]
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == opening:
            depth += 1
        elif char == closing:
            depth -= 1
            if depth == 0:
                return index
    return None


def unescape_markdown(value: str) -> str:
    return re.sub(r"\\([!\"#$%&'()*+,./:;<=>?@\[\\\]^_`{|}~-])", r"\1", value)


def inline_destinations(line: str):
    """Parse inline Markdown links/images, honoring escaping and nested parens."""
    index = 0
    while index < len(line):
        if line[index] == "\\":
            index += 2
            continue
        label_at = index + 1 if line[index:index + 2] == "![" else index
        if label_at >= len(line) or line[label_at] != "[":
            index += 1
            continue
        label_end = find_close(line, label_at + 1, "[", "]")
        if label_end is None or label_end + 1 >= len(line) or line[label_end + 1] != "(":
            index = label_at + 1
            continue
        pos = label_end + 2
        while pos < len(line) and line[pos] in " \t":
            pos += 1
        if pos < len(line) and line[pos] == "<":
            end = pos + 1
            escaped = False
            while end < len(line):
                if escaped:
                    escaped = False
                elif line[end] == "\\":
                    escaped = True
                elif line[end] == ">":
                    break
                end += 1
            if end >= len(line):
                index = label_end + 1
                continue
            destination = line[pos + 1:end]
            close = find_close(line, end + 1, "(", ")")
        else:
            end = pos
            depth = 0
            escaped = False
            while end < len(line):
                char = line[end]
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == "(":
                    depth += 1
                elif char == ")":
                    if depth == 0:
                        break
                    depth -= 1
                elif char in " \t" and depth == 0:
                    break
                end += 1
            destination = line[pos:end]
            close = find_close(line, end, "(", ")")
        if close is not None and destination:
            yield unescape_markdown(destination)
            index = close + 1
        else:
            index = label_end + 1


def github_slug(title: str) -> str:
    title = re.sub(r"<[^>]*>", "", title)
    title = html.unescape(title).strip().lower()
    title = "".join(ch for ch in title if ch.isalnum() or ch in " _-")
    return re.sub(r"[ \t]+", "-", title)


def anchors_for(text: str, name: str, findings: Findings) -> set[str]:
    anchors: set[str] = set()
    explicit: set[str] = set()
    slug_counts: Counter[str] = Counter()
    for line in unfenced_lines(text):
        for match in re.finditer(r"<a\s+[^>]*?\b(?:id|name)\s*=\s*(['\"])(.*?)\1[^>]*>", line, re.I):
            anchor = html.unescape(match.group(2))
            if contains_forbidden_control(anchor):
                findings.add(
                    "anchor-invalid",
                    name,
                    "explicit anchor contains a forbidden character",
                )
                continue
            if anchor in explicit:
                findings.add("anchor-duplicate", name, f"duplicate explicit anchor '{anchor}'")
            explicit.add(anchor)
            anchors.add(anchor)
        heading = re.match(r" {0,3}#{1,6}[ \t]+(.+?)[ \t]*#*[ \t]*$", line)
        if heading:
            base = github_slug(heading.group(1))
            if base:
                number = slug_counts[base]
                slug_counts[base] += 1
                anchors.add(base if number == 0 else f"{base}-{number}")
    return anchors


def exact_case_path(root: Path, relative: PurePosixPath) -> str | None:
    current = root
    actual: list[str] = []
    for component in relative.parts:
        try:
            names = os.listdir(current)
        except OSError:
            return None
        matches = sorted(name for name in names if name.casefold() == component.casefold())
        if not matches:
            return None
        chosen = component if component in matches else matches[0]
        actual.append(chosen)
        current /= chosen
    rendered = PurePosixPath(*actual).as_posix()
    return rendered if rendered != relative.as_posix() else None


def validate_destination(
    raw: str, source: Path, root: Path, docs: dict[Path, tuple[str, set[str]]], findings: Findings
) -> None:
    source_name = public_name(source, root)
    try:
        parts = urlsplit(raw)
        scheme = parts.scheme.lower()
        if scheme in SKIPPED_SCHEMES or raw.startswith("//") or raw.startswith("/"):
            return
        if scheme:  # Unknown schemes and drive-letter paths are not local documentation links.
            return
        decoded_path = unquote(parts.path, errors="strict").replace("\\", "/")
        fragment = unquote(parts.fragment, errors="strict")
    except (UnicodeError, ValueError):
        findings.add("link-invalid", source_name, "relative target has invalid URL encoding")
        return
    if contains_forbidden_control(decoded_path) or contains_forbidden_control(fragment):
        findings.add("link-invalid", source_name, "relative target contains a forbidden character")
        return
    relative = PurePosixPath(decoded_path) if decoded_path else PurePosixPath(source_name)
    if decoded_path:
        relative = PurePosixPath(source.parent.relative_to(root).as_posix()) / relative
    normalized_parts: list[str] = []
    escaped = False
    for component in relative.parts:
        if component in ("", "."):
            continue
        if component == "..":
            if not normalized_parts:
                escaped = True
                break
            normalized_parts.pop()
        else:
            normalized_parts.append(component)
    if escaped:
        encoded = parts.path.replace("\\", "/") != decoded_path
        message = (
            "percent-decoded relative target escapes documentation root"
            if encoded
            else "relative target escapes documentation root"
        )
        findings.add("link-traversal", source_name, message)
        return
    normalized = PurePosixPath(*normalized_parts)
    target = root.joinpath(*normalized.parts)
    try:
        resolved = target.resolve(strict=False)
        resolved.relative_to(root.resolve())
    except (OSError, ValueError):
        findings.add("link-traversal", source_name, "relative target escapes documentation root")
        return
    mismatch = exact_case_path(root, normalized)
    if mismatch:
        findings.add("link-case", source_name, f"target case does not match '{mismatch}'")
        return
    if not target.exists():
        findings.add("link-file", source_name, f"missing relative target '{normalized.as_posix()}'")
        return
    if target.is_dir():
        findings.add("link-file", source_name, f"relative target is not a file '{normalized.as_posix()}'")
        return
    if fragment and target.suffix.lower() == ".md":
        entry = docs.get(target)
        if entry is None:
            text = read_public(target, root, findings)
            if text is None:
                return
            entry = (text, anchors_for(text, normalized.as_posix(), findings))
            docs[target] = entry
        if fragment not in entry[1]:
            findings.add("link-anchor", source_name, f"missing anchor '{fragment}' in {normalized.as_posix()}")


def extract(pattern: str, text: str) -> str | None:
    match = re.search(pattern, text, re.M)
    return match.group(1) if match else None


def check_canonical(root: Path, findings: Findings) -> None:
    cargo_path, plugin_path = root / "Cargo.toml", root / "herdr-plugin.toml"
    if not (cargo_path.is_file() and plugin_path.is_file()):
        return
    try:
        cargo = tomllib.loads(read_bounded_utf8(cargo_path))["package"]
        plugin = tomllib.loads(read_bounded_utf8(plugin_path))
        version, rust, herdr = str(cargo["version"]), str(cargo["rust-version"]), str(plugin["min_herdr_version"])
    except (OSError, UnicodeError, BoundedTextTooLarge, tomllib.TOMLDecodeError, KeyError, TypeError):
        findings.add("canonical-source", "Cargo.toml", "cannot read package documentation contract")
        return
    if str(plugin.get("version", "")) != version:
        findings.add("package-version", "herdr-plugin.toml", f"version must match Cargo.toml {version}")
    documentation = str(cargo.get("documentation", ""))
    if documentation and f"/v{version}/" not in documentation:
        findings.add("package-version", "Cargo.toml", f"documentation URL must use v{version}")
    for relative in ("README.md", "docs/quickstart.md"):
        path = root / relative
        if not path.is_file():
            continue
        text = read_public(path, root, findings)
        if text is None:
            continue
        stable = extract(r"\*\*Stable v([0-9]+\.[0-9]+\.[0-9]+)\.\*\*", text)
        if stable != version:
            findings.add("package-version", relative, f"stable version must be v{version}")
        if not re.search(r"\*\*Development \(`main`\)\.\*\*[^\n]*(?:\n[^\n]*)?not\s+a\s+release", text, re.I):
            findings.add("stability-wording", relative, "main must be identified as development, not a release")
        expected = (("rust-requirement", "Rust", rust), ("tmux-requirement", "tmux", "3.3"), ("herdr-requirement", "Herdr", herdr))
        for category, label, value in expected:
            if not re.search(rf"(?:`?{re.escape(label)}`?)\s+{re.escape(value)}\s+or newer", text, re.I):
                findings.add(category, relative, f"minimum must be {label} {value} or newer")
    check_config_contract(root, findings)
    check_capability_contract(root, findings)


def check_config_contract(root: Path, findings: Findings) -> None:
    source_path, doc_path = root / "src/config.rs", root / "docs/configuration.md"
    if not (source_path.is_file() and doc_path.is_file()):
        return
    try:
        source = read_bounded_utf8(source_path)
        document = read_bounded_utf8(doc_path)
    except (OSError, UnicodeError, BoundedTextTooLarge):
        findings.add("config-default", "docs/configuration.md", "cannot compare public configuration defaults")
        return
    patterns = {
        "version": r"CURRENT_VERSION:\s*u32\s*=\s*(\d+)",
        "max_depth": r"max_depth:\s*(\d+)",
        "max_entries": r"max_entries:\s*(\d+)",
        "max_results": r"max_results:\s*(\d+)",
        "timeout_seconds": r"timeout_seconds:\s*(\d+)",
        "workers": r"workers:\s*(\d+)",
        "closed_days": r"closed_days:\s*(\d+)",
    }
    for key, pattern in patterns.items():
        value = extract(pattern, source)
        documented = extract(rf"^\s*{key}\s*=\s*(\d+)\s*$", document)
        if value is None or documented != value:
            findings.add("config-default", "docs/configuration.md", f"documented {key} must be {value or 'source-defined'}")
    if "placement: Placement::SplitRight" not in source or not re.search(r"^placement\s*=\s*\"split-right\"\s*$", document, re.M) or "`split-right` (default)" not in document:
        findings.add("placement-default", "docs/configuration.md", "split-right must be the documented configuration and placement default")


def check_capability_contract(root: Path, findings: Findings) -> None:
    source_path, doc_path = root / "src/orchestration.rs", root / "docs/architecture.md"
    if not (source_path.is_file() and doc_path.is_file()):
        return
    try:
        source, document = read_bounded_utf8(source_path), read_bounded_utf8(doc_path)
    except (OSError, UnicodeError, BoundedTextTooLarge):
        findings.add("capability-default", "docs/architecture.md", "cannot compare capability defaults")
        return
    source_default = bool(re.search(r"observe_output:\s*true\s*,\s*open_interactive:\s*true", source, re.S))
    documented = bool(re.search(r"defaults grant both\s+bounded observation and interactive open", document, re.I))
    if not source_default or not documented:
        findings.add("capability-default", "docs/architecture.md", "defaults must grant both bounded observation and interactive open")


def check(root: Path) -> list[str]:
    findings = Findings()
    try:
        root = root.resolve(strict=True)
    except OSError:
        return ["root <root>: documentation root is unavailable"]
    markdown = sorted((path for path in root.rglob("*.md") if ".git" not in path.parts and "target" not in path.parts), key=lambda p: public_name(p, root))
    if len(markdown) > MAX_DOCUMENTS:
        findings.add("document-count", "<root>", f"exceeds {MAX_DOCUMENTS} Markdown files")
        markdown = markdown[:MAX_DOCUMENTS]
    docs: dict[Path, tuple[str, set[str]]] = {}
    for path in markdown:
        try:
            path.resolve(strict=True).relative_to(root)
        except (OSError, ValueError):
            findings.add(
                "document-traversal",
                public_name(path, root),
                "Markdown file resolves outside documentation root",
            )
            continue
        text = read_public(path, root, findings)
        if text is not None:
            docs[path] = (text, anchors_for(text, public_name(path, root), findings))
    for path, (text, _) in sorted(docs.items(), key=lambda item: public_name(item[0], root)):
        for line in unfenced_lines(text):
            for destination in inline_destinations(line):
                validate_destination(destination, path, root, docs, findings)
    check_canonical(root, findings)
    return findings.output()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    diagnostics = check(args.root)
    if diagnostics:
        print("\n".join(diagnostics))
        return 1
    print("documentation contracts: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
