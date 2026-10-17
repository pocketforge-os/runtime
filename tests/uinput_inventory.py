#!/usr/bin/env python3
"""Reject unclassified kernel-input creation/exposure occurrences and host-route violations.

Part 1 (creator inventory) is the original uinput scan: every /dev/uinput,
UI_CREATE, UInput/VirtualDevice construction or container /dev/input device
occurrence must be classified in tests/uinput-inventory.txt.

Part 2 (host-route policy, tsp-ihne1.56.5) is the canonical static spec from
design-plan.md sections 3-4: the seven host-route rule families, the typed
private-compositor / guest_internal / product_session_capability exceptions,
the fail-closed dynamic_route rule, and the runner-label (runs-on) evaluator
with GitHub subset semantics. Shell, YAML and systemd values are tokenised, so
continuation and whitespace spellings are caught. Other repositories vendor
tests/fixtures/host-route-policy-v1.json and host-route-cases-v1.jsonl plus a
.SOURCE pin; this file is the canonical behavior those ports reproduce.
"""

import argparse
from collections import Counter
import shlex
import fnmatch
import hashlib
import json
from pathlib import Path
import re
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "tests/uinput-inventory.txt"
SOURCE_SUFFIXES = {".c", ".h", ".py", ".rs", ".sh", ".toml", ".service", ".yml", ".yaml"}
PATTERN = re.compile(r"/dev/uinput|\bUI_(?:DEV_)?CREATE\b|\bUInput\s*\(|\bVirtualDevice(?:::\w+)?\s*\(|(?:--device|-v|--mount)[^\n]*/dev/input")

HOST_ROUTE_TAG = "\t# host-route "
POLICY_RELPATH = "tests/fixtures/host-route-policy-v1.json"
CASES_RELPATH = "tests/fixtures/host-route-cases-v1.jsonl"
MAP_RELPATH = "tests/fixtures/input-runner-host-map-v1.json"
MAP_SCHEMA_RELPATH = "tests/fixtures/input-runner-host-map-v1.schema.json"
MAP_SOURCE_RELPATH = "tests/fixtures/input-runner-host-map-v1.SOURCE"
MANIFEST_RELPATH = "tests/uinput-inventory.txt"


def fingerprint(path, line):
    digest = hashlib.sha256(line.strip().encode()).hexdigest()
    return f"{path}\t{digest}"


def tracked(root=None):
    root = ROOT if root is None else root
    paths = []
    for path in root.rglob("*"):
        rel = path.relative_to(root)
        if not path.is_file() or any(part in {".git", "vendor", "target", "build"} for part in rel.parts):
            continue
        paths.append(rel)
    return sorted(paths)


def occurrences(extra=None):
    found = []
    for rel in tracked():
        if rel.parts[0] == "vendor" or rel.name in {MANIFEST.name, Path(__file__).name}:
            continue
        if rel.suffix not in SOURCE_SUFFIXES and rel.name != "Makefile":
            continue
        try:
            lines = (ROOT / rel).read_text(encoding="utf-8").splitlines()
        except UnicodeDecodeError:
            continue
        for number, line in enumerate(lines, 1):
            if PATTERN.search(line):
                found.append((fingerprint(rel.as_posix(), line), rel.as_posix(), number, line.strip()))
    if extra:
        for path, line in extra:
            found.append((fingerprint(path, line), path, 1, line))
    return found


def load_manifest(path=None):
    path = MANIFEST if path is None else path
    return Counter(line.split("\t#", 1)[0] for line in path.read_text().splitlines()
                   if line and not line.startswith("#") and HOST_ROUTE_TAG not in line)


def check(extra=None, quiet=False, allowed=None):
    allowed = load_manifest() if allowed is None else allowed
    items = occurrences(extra)
    live = Counter(item[0] for item in items)
    unclassified = live - allowed
    stale = allowed - live
    if not quiet:
        by_key = {}
        for item in items:
            by_key.setdefault(item[0], []).append(item)
        for key, count in unclassified.items():
            for _, path, number, line in by_key[key][-count:]:
                print(f"unclassified uinput occurrence: {path}:{number}: {line}", file=sys.stderr)
        for key, count in stale.items():
            print(f"stale uinput inventory entry ({count}): {key}", file=sys.stderr)
    return not unclassified and not stale


# ---------------------------------------------------------------------------
# Host-route policy: findings, shell lexer, YAML subset parser, matchers.
# ---------------------------------------------------------------------------

class MalformedFile(Exception):
    pass


class Finding:
    """A candidate host-route occurrence before exception resolution."""

    def __init__(self, rule, detail, path, line, text, excusable=frozenset(),
                 route_source=None, route_destination=None):
        self.rule = rule
        self.detail = detail
        self.path = path
        self.line = line
        self.text = text.strip()[:200]
        self.excusable = frozenset(excusable)
        self.excused = False
        self.kind = "source"
        self.job = None
        self.route_source = route_source
        self.route_destination = route_destination

    @property
    def key(self):
        digest = hashlib.sha256(f"{self.rule}\n{self.text}".encode()).hexdigest()
        return f"{self.path}\t{digest}"

    def render(self):
        return (f"host_route_inventory=FAIL reason={self.rule} path={self.path} "
                f"line={self.line} rule={self.rule} detail={self.detail} text={self.text!r}")

    def __repr__(self):
        return f"Finding({self.rule}/{self.detail} {self.path}:{self.line})"


class Tok:
    __slots__ = ("text", "line", "quoted")

    def __init__(self, text, line, quoted=False):
        self.text = text
        self.line = line
        self.quoted = quoted

    def __repr__(self):
        return f"Tok({self.text!r}, line={self.line})"


SEPARATORS = {";", "|", "&&", "||", "&", "\n"}


def skip_command_substitution(text, i, line):
    """Skip a $( ... ) span starting at text[i] ('$' at i, '(' at i + 1).

    Returns (index just past the closing paren, updated line counter).
    Quote spans inside the substitution are skipped, and a backslash outside
    a quote escapes the next character, so nested or literal quotes cannot
    unbalance the scan (canonical-spec amendment ruling 3: image
    test-kbd-layout-env.sh:193 and test-shell-fonts.sh:87 spellings).
    """
    n = len(text)
    depth = 0
    j = i + 1
    while j < n:
        c = text[j]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return j + 1, line
        elif c == "\\":
            if j + 1 < n and text[j + 1] == "\n":
                line += 1
            j += 2
            continue
        elif c == "'":
            j += 1
            while j < n and text[j] != "'":
                if text[j] == "\n":
                    line += 1
                j += 1
        elif c == '"':
            j += 1
            while j < n and text[j] != '"':
                if text[j] == "\\":
                    j += 1
                if j < n and text[j] == "\n":
                    line += 1
                j += 1
        elif c == "\n":
            line += 1
        j += 1
    raise MalformedFile(f"unbalanced command substitution at line {line}")


def lex_shell_text(text, base_line=1):
    """Tokenise shell text into logical commands.

    Returns (commands, heredocs). A command is (start_line, [Tok]); separator
    operators split commands. Heredoc bodies are returned as (start_line,
    delimiter, text) chunks so callers can scan embedded scripts. Quotes,
    backslash continuations and command substitutions are tracked, so
    continuation and whitespace spellings normalise to the same tokens.
    Raises MalformedFile on unbalanced quotes (fail closed).
    """
    commands = []
    heredocs = []
    current = []
    current_line = base_line
    buf = []
    buf_start = base_line
    buf_quoted = [False]
    i = 0
    n = len(text)
    line = base_line
    pending_heredocs = []

    def flush_token(end_line):
        if buf:
            current.append(Tok("".join(buf), buf_start, buf_quoted[0]))
            del buf[:]
            buf_quoted[0] = False

    def flush_command(end_line):
        nonlocal current, current_line
        flush_token(end_line)
        if current:
            commands.append((current_line, current))
            current = []
        current_line = end_line

    while i < n:
        ch = text[i]
        if ch == "\n":
            flush_command(line)
            i += 1
            line += 1
            if pending_heredocs:
                for delim, strip_tabs in pending_heredocs:
                    body = []
                    body_start = line
                    while i < n:
                        j = text.find("\n", i)
                        raw = text[i:] if j < 0 else text[i:j]
                        cand = raw.lstrip("\t") if strip_tabs else raw
                        if cand == delim:
                            i = (j + 1) if j >= 0 else n
                            line += 1
                            break
                        body.append(raw)
                        i = (j + 1) if j >= 0 else n
                        line += 1
                        if j < 0:
                            break
                    heredocs.append((body_start, delim, "\n".join(body)))
                pending_heredocs = []
            current_line = line
            continue
        if ch == "\\" and i + 1 < n and text[i + 1] == "\n":
            if not buf:
                buf_start = line
            i += 2
            line += 1
            continue
        if ch == "#" and not buf:
            while i < n and text[i] != "\n":
                i += 1
            continue
        if ch == "'":
            if not buf:
                buf_start = line
            buf_quoted[0] = True
            i += 1
            while True:
                if i >= n:
                    raise MalformedFile(f"unbalanced single quote at line {line}")
                if text[i] == "'":
                    i += 1
                    break
                if text[i] == "\n":
                    line += 1
                buf.append(text[i])
                i += 1
            continue
        if ch == '"':
            if not buf:
                buf_start = line
            buf_quoted[0] = True
            i += 1
            while True:
                if i >= n:
                    raise MalformedFile(f"unbalanced double quote at line {line}")
                if text[i] == '"':
                    i += 1
                    break
                if text[i] == "\\" and i + 1 < n:
                    nxt = text[i + 1]
                    if nxt == "\n":
                        line += 1
                        i += 2
                        continue
                    if nxt in '$`"\\':
                        buf.append(nxt)
                        i += 2
                        continue
                if text[i] == "$" and i + 1 < n and text[i + 1] == "(":
                    # Command substitution inside double quotes: nested
                    # quotes belong to the substitution, not to this string
                    # (canonical-spec amendment ruling 3).
                    j, line = skip_command_substitution(text, i, line)
                    buf.append(text[i:j])
                    i = j
                    continue
                if text[i] == "`":
                    j = text.find("`", i + 1)
                    if j < 0:
                        raise MalformedFile(f"unbalanced backtick at line {line}")
                    buf.append(text[i:j + 1])
                    line += text.count("\n", i, j + 1)
                    i = j + 1
                    continue
                if text[i] == "\n":
                    line += 1
                buf.append(text[i])
                i += 1
            continue
        if ch == "`":
            if not buf:
                buf_start = line
            j = text.find("`", i + 1)
            if j < 0:
                raise MalformedFile(f"unbalanced backtick at line {line}")
            buf.append(text[i:j + 1])
            line += text.count("\n", i, j + 1)
            i = j + 1
            continue
        if ch == "$" and i + 1 < n and text[i + 1] == "(":
            if not buf:
                buf_start = line
            j, line = skip_command_substitution(text, i, line)
            buf.append(text[i:j])
            i = j
            continue
        if ch == "$" and i + 1 < n and text[i + 1] == "'":
            # ANSI-C quote $'...': backslash escapes the next character, so
            # $'it\'s' is one balanced token (canonical-spec amendment
            # ruling 3).
            if not buf:
                buf_start = line
            buf_quoted[0] = True
            j = i + 2
            closed = False
            while j < n:
                c = text[j]
                if c == "\\":
                    if j + 1 < n and text[j + 1] == "\n":
                        line += 1
                    j += 2
                    continue
                if c == "'":
                    j += 1
                    closed = True
                    break
                if c == "\n":
                    line += 1
                j += 1
            if not closed:
                raise MalformedFile(f"unbalanced ANSI-C quote at line {line}")
            buf.append(text[i:j])
            i = j
            continue
        if ch == "<" and text.startswith("<<", i):
            flush_token(line)
            strip_tabs = text.startswith("<<-", i)
            i += 3 if strip_tabs else 2
            while i < n and text[i] in " \t":
                i += 1
            quote = None
            if i < n and text[i] in "\"'":
                quote = text[i]
                i += 1
            start = i
            while i < n and (text[i].isalnum() or text[i] in "_-"):
                i += 1
            delim = text[start:i]
            if quote and i < n and text[i] == quote:
                i += 1
            if delim:
                pending_heredocs.append((delim, strip_tabs))
            continue
        if ch in ";&|()<>":
            two = text[i:i + 2]
            op = two if two in {"&&", "||"} else ch
            flush_token(line)
            if op in SEPARATORS:
                if current:
                    commands.append((current_line, current))
                    current = []
                current_line = line
            else:
                current.append(Tok(op, line))
            i += len(op)
            continue
        if ch in " \t":
            flush_token(line)
            i += 1
            continue
        if not buf:
            buf_start = line
        buf.append(ch)
        i += 1
    flush_command(line)
    return commands, heredocs


def lex_tolerant(text, line):
    """Tokenise one logical line without raising: quotes are honoured where
    balanced, otherwise the line falls back to whitespace splitting."""
    try:
        commands, _ = lex_shell_text(text, line)
        toks = []
        for _, cmd in commands:
            toks.extend(cmd)
        return toks if toks else [Tok(part, line) for part in text.split()]
    except MalformedFile:
        return [Tok(part, line) for part in text.split()]


SOURCE_SUFFIXES_HR = {".py", ".rs", ".c", ".h", ".toml"}
SYSTEMD_SUFFIXES = {".service", ".socket", ".mount", ".target", ".conf"}
YAML_SUFFIXES = {".yml", ".yaml"}
SHELL_NAMES = {"Makefile", "Dockerfile", "Containerfile"}


def host_route_file_kind(rel, head):
    """Classify a tracked file for host-route scanning. Documentation
    (.md), data fixtures (.json/.jsonl/.tsv) and the scanner/manifest itself
    are never scanned (docs-only text is a positive control)."""
    name = rel.name
    posix = rel.as_posix()
    if name in {MANIFEST.name, Path(__file__).name}:
        return None
    if posix.startswith("tests/fixtures/"):
        return None
    base = name
    while base.endswith(".in"):
        base = base[:-3]
    suffix = Path(base).suffix
    if posix.startswith(".github/workflows/") and suffix in YAML_SUFFIXES:
        return "workflow"
    if name.endswith((".yml.in", ".yaml.in")):
        return "workflow"
    if suffix in YAML_SUFFIXES:
        return "yaml"
    if suffix in {".sh", ".bash"} or base in SHELL_NAMES:
        return "shell"
    if suffix in SYSTEMD_SUFFIXES:
        return "systemd"
    if suffix in SOURCE_SUFFIXES_HR:
        return "source"
    if suffix == "" and head.startswith("#!"):
        first = head.splitlines()[0] if head.splitlines() else head
        if re.search(r"\b(sh|bash|dash)\b", first):
            return "shell"
        if "python" in first:
            return "source"
    return None


STRING_LITERAL = re.compile(r"\"([^\"\\\n]*(?:\\.[^\"\\\n]*)*)\"|'([^'\\\n]*(?:\\.[^'\\\n]*)*)'")
SOURCE_ROUTE_PAIR = re.compile(
    r"\(\s*[\"'](?P<source>/[^\"'\r\n]*)[\"']\s*,\s*"
    r"[\"'](?P<destination>/[^\"'\r\n]*)[\"']\s*\)"
)


def string_literals(line):
    out = []
    for match in STRING_LITERAL.finditer(line):
        out.append(match.group(1) if match.group(1) is not None else match.group(2))
    return out


def join_logical_lines(lines, base_line=1):
    """Join backslash continuations, yielding (first_line_no, joined_text)."""
    out = []
    buf = None
    start = base_line
    for offset, raw in enumerate(lines):
        line_no = base_line + offset
        if buf is None:
            start = line_no
        candidate = raw.rstrip()
        if candidate.endswith("\\") and not candidate.endswith("\\\\"):
            buf = (buf or "") + candidate[:-1] + " "
            continue
        out.append((start, (buf or "") + raw))
        buf = None
    if buf is not None:
        out.append((start, buf))
    return out


# --- YAML subset parser (GitHub workflows / compose), stdlib only ----------

class YNode:
    __slots__ = ("value", "line", "style", "key_lines", "body_line")

    def __init__(self, value, line, style="plain", key_lines=None, body_line=None):
        self.value = value
        self.line = line
        self.style = style
        self.key_lines = key_lines or {}
        self.body_line = body_line

    def get(self, key):
        if isinstance(self.value, dict):
            return self.value.get(key)
        return None


def _strip_comment(content):
    out = []
    quote = None
    i = 0
    while i < len(content):
        ch = content[i]
        if quote:
            out.append(ch)
            if ch == "\\" and quote == '"' and i + 1 < len(content):
                out.append(content[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(ch)
        elif ch == "#" and (not out or out[-1] in " \t"):
            break
        else:
            out.append(ch)
        i += 1
    return "".join(out).rstrip()


def _unquote(text):
    text = text.strip()
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "\"'":
        inner = text[1:-1]
        if text[0] == '"':
            inner = inner.replace('\\"', '"').replace("\\\\", "\\")
        return inner
    return text


def _flow_complete(text):
    depth = 0
    quote = None
    i = 0
    while i < len(text):
        ch = text[i]
        if quote:
            if ch == "\\" and quote == '"':
                i += 2
                continue
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch in "[{":
            depth += 1
        elif ch in "]}":
            depth -= 1
            if depth == 0:
                return True
        i += 1
    return depth == 0 and quote is None


def _split_flow_items(inner):
    items = []
    depth = 0
    quote = None
    cur = []
    i = 0
    while i < len(inner):
        ch = inner[i]
        if quote:
            cur.append(ch)
            if ch == "\\" and quote == '"' and i + 1 < len(inner):
                cur.append(inner[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            cur.append(ch)
        elif ch in "[{":
            depth += 1
            cur.append(ch)
        elif ch in "]}":
            depth -= 1
            cur.append(ch)
        elif ch == "," and depth == 0:
            items.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
        i += 1
    items.append("".join(cur))
    return items


def _parse_flow(text, line):
    text = text.strip()
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        items = [it for it in _split_flow_items(inner) if it.strip()] if inner else []
        return YNode([_parse_flow(it.strip(), line) if it.strip()[:1] in "[{"
                      else YNode(_unquote(it.strip()), line) for it in items], line, "flowseq")
    if text.startswith("{") and text.endswith("}"):
        inner = text[1:-1].strip()
        mapping = {}
        if inner:
            for part in _split_flow_items(inner):
                if not part.strip():
                    continue
                if ":" not in part:
                    raise MalformedFile(f"flow mapping entry without ':' at line {line}")
                k, v = part.split(":", 1)
                mapping[_unquote(k.strip())] = YNode(_unquote(v.strip()), line)
        return YNode(mapping, line, "flowmap")
    return YNode(_unquote(text), line)


class YamlParser:
    def __init__(self, text):
        self.raw_lines = text.splitlines()
        self.lines = []  # mutable [idx, lineno, indent, content]
        for idx, raw in enumerate(self.raw_lines):
            lead = raw[:len(raw) - len(raw.lstrip())]
            if "\t" in lead:
                raise MalformedFile(f"tab indentation at line {idx + 1}")
            stripped = raw.strip()
            if not stripped or stripped.startswith("#"):
                continue
            indent = len(lead)
            self.lines.append([idx, idx + 1, indent, _strip_comment(raw[indent:])])
        self.pos = 0

    def peek(self):
        return self.lines[self.pos] if self.pos < len(self.lines) else None

    def parse(self):
        node = self.parse_block(0)
        if self.peek() is not None:
            raise MalformedFile(f"trailing content at line {self.peek()[1]}")
        return node if node is not None else YNode({}, 1)

    def parse_block(self, indent):
        entry = self.peek()
        if entry is None or entry[2] < indent:
            return None
        content = entry[3]
        if content == "-" or content.startswith("- "):
            return self.parse_sequence(entry[2])
        return self.parse_mapping(entry[2])

    def parse_sequence(self, indent):
        items = []
        first_line = self.peek()[1]
        while True:
            entry = self.peek()
            if entry is None or entry[2] != indent:
                break
            content = entry[3]
            if not (content == "-" or content.startswith("- ")):
                break
            rest = content[1:].strip() if content != "-" else ""
            if not rest:
                self.pos += 1
                child = self.parse_deeper(indent)
                items.append(child if child is not None else YNode(None, entry[1], "empty"))
                continue
            rest_indent = entry[2] + (len(content[1:]) - len(content[1:].lstrip())) + 1
            key, key_rest = self.split_key(rest)
            if key is None and not rest.startswith("- "):
                # Sequence item is a plain/quoted scalar or a flow collection.
                self.pos += 1
                text = rest
                lineno = entry[1]
                if text[0] in "[{" and not _flow_complete(text):
                    while not _flow_complete(text):
                        nxt = self.peek()
                        if nxt is None:
                            raise MalformedFile(f"unbalanced flow collection at line {lineno}")
                        text += " " + nxt[3]
                        self.pos += 1
                if text[0] in "[{":
                    items.append(_parse_flow(text, lineno))
                elif text[0] in "|>":
                    items.append(self.parse_block_scalar(text, rest_indent, lineno))
                else:
                    items.append(YNode(_unquote(text), lineno))
                continue
            self.lines[self.pos][2] = rest_indent
            self.lines[self.pos][3] = rest
            child = self.parse_block(rest_indent)
            items.append(child if child is not None else YNode(rest, entry[1]))
        return YNode(items, first_line, "seq")

    def parse_deeper(self, indent):
        entry = self.peek()
        if entry is None or entry[2] <= indent:
            return None
        return self.parse_block(entry[2])

    def parse_mapping(self, indent):
        mapping = {}
        key_lines = {}
        first_line = self.peek()[1]
        while True:
            entry = self.peek()
            if entry is None or entry[2] != indent:
                break
            content = entry[3]
            if content == "-" or content.startswith("- "):
                break
            key, rest = self.split_key(content)
            if key is None:
                raise MalformedFile(f"mapping line without ':' at line {entry[1]}: {content!r}")
            lineno = entry[1]
            self.pos += 1
            key_lines[key] = lineno
            rest = rest.strip()
            if rest and rest[0] in "|>":
                mapping[key] = self.parse_block_scalar(rest, indent, lineno)
            elif rest:
                text = rest
                if text[0] in "[{" and not _flow_complete(text):
                    while not _flow_complete(text):
                        nxt = self.peek()
                        if nxt is None:
                            raise MalformedFile(f"unbalanced flow collection at line {lineno}")
                        text += " " + nxt[3]
                        self.pos += 1
                if text[0] in "[{":
                    mapping[key] = _parse_flow(text, lineno)
                else:
                    mapping[key] = YNode(_unquote(text), lineno,
                                         "dq" if text.startswith('"') else
                                         "sq" if text.startswith("'") else "plain")
            else:
                child = self.parse_deeper(indent)
                if child is None:
                    mapping[key] = YNode(None, lineno, "empty")
                else:
                    mapping[key] = child
        return YNode(mapping, first_line, "map", key_lines)

    def split_key(self, content):
        quote = None
        depth = 0
        i = 0
        while i < len(content):
            ch = content[i]
            if quote:
                if ch == "\\" and quote == '"':
                    i += 2
                    continue
                if ch == quote:
                    quote = None
            elif ch in "\"'":
                quote = ch
            elif ch in "[{":
                depth += 1
            elif ch in "]}":
                depth -= 1
            elif ch == ":" and depth == 0:
                if i + 1 == len(content) or content[i + 1] in " \t":
                    return _unquote(content[:i]), content[i + 1:]
            i += 1
        return None, None

    def parse_block_scalar(self, header, indent, lineno):
        style = "literal" if header[0] == "|" else "folded"
        body = []
        body_line = None
        while self.pos < len(self.lines):
            entry = self.lines[self.pos]
            if entry[2] <= indent:
                break
            if body_line is None:
                body_line = entry[1]
            body.append(self.raw_lines[entry[0]])
            self.pos += 1
        text = "\n".join(body)
        return YNode(text, lineno, style, body_line=body_line)


def parse_yaml_subset(text):
    return YamlParser(text).parse()


# --- Policy ----------------------------------------------------------------

CANONICAL_RULES = (
    "host_uinput_device", "host_input_bind", "input_major_wildcard",
    "host_uinput_permission", "host_display_projection",
    "host_clipboard_write", "dynamic_route",
)
CANONICAL_CLASSES = {
    "dormant-k-consumer": ["wave2"],
    "product-reference": [],
    "product-session-fixture": [],
    "guest_internal": ["caller"],
    "product_clipboard_adapter": [],
    "product_clipboard_adapter_call": [],
}
DEFAULT_GUEST_ENTRY_POINTS = ["*/run-guest.sh", "run-guest.sh", "*/guest/*"]
DEFAULT_CLIPBOARD_GATE = "host_clipboard_disabled"
DEFAULT_PRODUCER_X11_SOURCE = "/run/pocketforge/x11/.X11-unix/X0"
DEFAULT_PRODUCER_X11_DESTINATION = "/tmp/.X11-unix/X0"


class Policy:
    def __init__(self, doc, failures):
        self.doc = doc
        rules = tuple(r.get("id") for r in doc.get("rules", [])) or CANONICAL_RULES
        if set(rules) != set(CANONICAL_RULES):
            failures.append(Finding("-", "policy_mismatch", POLICY_RELPATH, 1,
                                    "policy rule ids diverge from the canonical implementation"))
        self.rules = rules
        classes = {k: v.get("required_fields", [])
                   for k, v in doc.get("inventory", {}).get("classes", {}).items()} or dict(CANONICAL_CLASSES)
        if set(classes) != set(CANONICAL_CLASSES):
            failures.append(Finding("-", "policy_mismatch", POLICY_RELPATH, 1,
                                    "policy inventory classes diverge from the canonical implementation"))
        self.classes = classes
        typed = doc.get("typed_exceptions", {})
        self.guest_entry_points = typed.get("guest_internal", {}).get(
            "guest_entry_points", DEFAULT_GUEST_ENTRY_POINTS)
        self.clipboard_gate_marker = typed.get("product_clipboard_adapter", {}).get(
            "gate_marker", DEFAULT_CLIPBOARD_GATE)
        self.producer_x11_source = typed.get("product_session_capability", {}).get(
            "producer_x11_source", DEFAULT_PRODUCER_X11_SOURCE)
        self.producer_x11_destination = typed.get("product_session_capability", {}).get(
            "producer_x11_destination", DEFAULT_PRODUCER_X11_DESTINATION)


def load_policy(root, failures):
    path = root / POLICY_RELPATH
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        failures.append(Finding("-", "policy_mismatch", POLICY_RELPATH, 1, f"unreadable policy: {exc}"))
        doc = {}
    return Policy(doc, failures)


# --- Matchers ---------------------------------------------------------------

DISPLAY_VARS = ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY")
CONTAINER_PROGS = {"docker", "podman"}
PERMISSION_PROGS = {"chmod", "chgrp", "chown", "setfacl", "mknod", "install"}
DISPLAY_TOOLS = ("xdotool", "xhost", "xwd")
CLIPBOARD_TOOLS = ("wl-copy", "xclip", "xsel")
CLIPBOARD_LIBS = ("arboard", "copypasta", "x11-clipboard", "wl-clipboard-rs", "gtk::Clipboard")
CLIPBOARD_API = re.compile(r"\bcopy_text\s*\(|\bcopy_image\s*\(")
DEV_UINPUT = re.compile(r"^/dev/uinput$")
DEV_INPUT = re.compile(r"^/dev/input(/|$)")
HOST_PATHS = [
    (re.compile(r"^/tmp/\.X11-unix(/|$)"), "x11_socket_path"),
    (re.compile(r"(^|/)\.Xauthority$"), "xauthority_path"),
    (re.compile(r"^/run/user/[^/]+/wayland-"), "wayland_socket_path"),
]
WAYLAND_ENV_BIND = re.compile(r"XDG_RUNTIME_DIR.*WAYLAND_DISPLAY|WAYLAND_DISPLAY.*XDG_RUNTIME_DIR")
MAJOR13 = re.compile(r"^[abc]\s+13\s*:")
INHERITED_REF = re.compile(r"\$\{?(DISPLAY|WAYLAND_DISPLAY|XAUTHORITY)\b")
GITHUB_EXPR = re.compile(r"\$\{\{([^}]*)\}\}")
VAR_REF = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)")
SOURCE_SETENV = re.compile(
    r"\bset_var\s*\(\s*\"(DISPLAY|WAYLAND_DISPLAY|XAUTHORITY)\"|"
    r"os\.environ\[\s*[\"'](DISPLAY|WAYLAND_DISPLAY|XAUTHORITY)[\"']\s*\]\s*=|"
    r"\bsetenv\s*\(\s*\"(DISPLAY|WAYLAND_DISPLAY|XAUTHORITY)\"")
# --env-file is deliberately NOT skippable: any --env-file fails closed
# (#153) because the file can carry DISPLAY/WAYLAND_DISPLAY/XAUTHORITY.
SKIP_VALUE_FLAGS = {"--label", "-l", "--name", "--user", "-u", "--workdir", "-w",
                    "--entrypoint", "--network", "--pid", "--ipc", "--cpus", "--memory", "-m",
                    "--shm-size", "--tmpfs", "--stop-signal", "--platform", "--pull", "-t", "--tag"}
# Docker/podman global options (before the subcommand) and run/create option
# arities. A text scan cannot prove the arity of an option it does not know, so
# known arities are parsed and unknown options taint the rest of the command:
# the parser keeps scanning and fails closed on any dynamic operand (#147
# review: an unknown split-value option let the value be mistaken for the
# image, hiding the real image operand).
CONTAINER_GLOBAL_VALUE_FLAGS = {"--config", "--context", "-c", "--host", "-H", "--log-level",
                                "--tlscacert", "--tlscert", "--tlskey", "--tlsversion"}
CONTAINER_GLOBAL_BOOL_FLAGS = {"-D", "--debug", "--tls", "--tlsverify"}
# run/create booleans; note -t is --tty here (not --tag). --pull is NOT a
# boolean: docker and podman both declare `--pull string` for run and create
# and accept the value as a split token, so it consumes split or inline values
# via RUN_VALUE_FLAGS (#147 review round 2: `docker run --pull always "$IMAGE"`
# mistook `always` for the image).
RUN_BOOL_FLAGS = {"--rm", "--init", "-d", "--detach", "-i", "--interactive", "-t", "--tty",
                  "--privileged", "--read-only", "--no-healthcheck", "-P", "--publish-all",
                  "-q", "--quiet", "--sig-proxy", "--oom-kill-disable"}
RUN_SHORT_BOOL_CHARS = set("itdPq")
# Short options that take a value inside run/create/exec; an attached value
# (-eDISPLAY=:0, -v/dev/input:/x) decomposes against these (#153).
SHORT_VALUE_CHARS = set("evuwlpmh")
RUN_VALUE_FLAGS = (SKIP_VALUE_FLAGS | {
    "--hostname", "-h", "--add-host", "--runtime", "--cap-add", "--cap-drop", "--dns",
    "--dns-option", "--dns-search", "--domainname", "--expose", "--publish", "-p",
    "--restart", "--sysctl", "--ulimit", "--security-opt", "--log-driver", "--log-opt",
    "--gpus", "--group-add", "--link", "--link-local-ip", "--net", "--net-alias",
    "--mac-address", "--ip", "--ip6", "--cpuset-cpus", "--cpuset-mems", "--cpu-shares",
    "--cpu-period", "--cpu-quota", "--cpu-rt-period", "--cpu-rt-runtime",
    "--cgroup-parent", "--cidfile", "--label-file", "--memory-reservation",
    "--memory-swap", "--memory-swappiness", "--blkio-weight", "--detach-keys",
    "--health-cmd", "--health-interval", "--health-retries", "--health-start-period",
    "--health-timeout", "--isolation", "--kernel-memory", "--oom-score-adj",
    "--pids-limit", "--stop-timeout", "--storage-opt", "--userns", "--uts",
    "--volume-driver", "--volumes-from", "--device-read-bps", "--device-read-iops",
    "--device-write-bps", "--device-write-iops", "--annotation"}) - {"-t", "--tag"}


class Resolver:
    """Resolves shell variables from single literal assignments in a boundary."""

    def __init__(self):
        self.values = {}
        self.conflicts = set()

    def record(self, name, value):
        if name in self.values and self.values[name] != value:
            self.conflicts.add(name)
        self.values[name] = value

    def resolve(self, text, depth=0):
        if text is None:
            return None
        if depth > 4:
            return None
        out = text
        for match in VAR_REF.finditer(text):
            name = match.group(1)
            default = None
            if match.group(0).startswith("${") and ":-" in match.group(0):
                default = match.group(0).split(":-", 1)[1].rstrip("}")
            if name in self.conflicts:
                return None
            val = self.values.get(name)
            if val is None:
                if default is not None:
                    val = default
                else:
                    return None
            resolved = self.resolve(val, depth + 1)
            if resolved is None:
                return None
            out = out.replace(match.group(0), resolved)
        if "$" in out or "`" in out:
            return None
        return out

    def is_generated(self, text):
        """True when the value is generated by mktemp (run-private)."""
        if text is None:
            return False
        if "mktemp" in text:
            return True
        for match in VAR_REF.finditer(text):
            val = self.values.get(match.group(1))
            if val and "mktemp" in val:
                return True
        return False


class Evidence:
    """Private-compositor creation evidence inside one test boundary."""

    def __init__(self):
        self.xvfb_run = False
        self.xvfb_displays = set()
        self.weston_sockets = set()
        self.weston_socket_vars = set()
        self.weston_headless = False
        self.xdg_runtime_private = False
        self.cleanup_owned = False


class Boundary:
    def __init__(self, path, kind, resolver=None, evidence=None, text=""):
        self.path = path
        self.kind = kind
        self.resolver = resolver or Resolver()
        self.evidence = evidence or Evidence()
        self.text = text
        self._lines = text.splitlines()

    def line_text(self, line_no):
        if 0 < line_no <= len(self._lines):
            return self._lines[line_no - 1].strip()
        return ""


def is_host_path(path):
    if DEV_UINPUT.match(path) or DEV_INPUT.match(path):
        return True
    return any(pattern.search(path) for pattern, _ in HOST_PATHS)


def is_dynamic_value(val):
    """True when val contains a shell expansion ($ or backtick).

    pf-hwprobe#23 round-4 adjudication: a text scan cannot prove a shell
    variable's runtime value. printf -v, read, eval, sourced scripts and
    $GITHUB_ENV writes all bypass assignment folding, so a dynamic value is
    never excused by resolving it against same-file assignments.
    """
    return val is not None and ("$" in val or "`" in val)


def check_device_value(val, boundary, findings, line, text, container_ctx=True):
    if val is None:
        return
    if is_dynamic_value(val):
        # No assignment folding: a dynamic value in a container context is
        # always dynamic_route. Outside a container context folding is used
        # to indict only (never to excuse).
        if container_ctx:
            findings.append(Finding("dynamic_route", "unresolved_value", boundary.path, line, text))
        else:
            resolved = boundary.resolver.resolve(val)
            if resolved is not None \
                    and (DEV_UINPUT.match(resolved) or DEV_INPUT.match(resolved)):
                findings.append(Finding("host_uinput_device", "device_flag", boundary.path,
                                        line, text))
        return
    if DEV_UINPUT.match(val) or DEV_INPUT.match(val):
        findings.append(Finding("host_uinput_device", "device_flag", boundary.path, line, text))


WORKSPACE_ANCHORS = (
    "$GITHUB_WORKSPACE", "${GITHUB_WORKSPACE}",
    "$RUNNER_TEMP", "${RUNNER_TEMP}",
    "${{ github.workspace }}", "${{ runner.temp }}",
)


def workspace_anchored(src):
    """True when a dynamic bind source is anchored at a runner workspace root.

    Canonical-spec amendment ruling 1: $GITHUB_WORKSPACE/$RUNNER_TEMP (and
    their ${{ ... }} workflow-expression forms) are provisioned per job and
    torn down with it, so a bind source that begins at one of these roots --
    followed by end of word or `/`, with no literal `..` segment anywhere --
    cannot reach a host input, display or clipboard route. Every other
    dynamic source stays dynamic_route.
    """
    if any(segment == ".." for segment in src.split("/")):
        return False
    for anchor in WORKSPACE_ANCHORS:
        if src == anchor:
            return True
        if src.startswith(anchor) and src[len(anchor):].startswith("/"):
            return True
    return False


def check_volume_value(val, boundary, findings, line, text):
    if val is None:
        return
    if WAYLAND_ENV_BIND.search(val):
        findings.append(Finding("host_input_bind", "bind_source", boundary.path, line, text))
        return
    src = val.split(":")[0]
    if is_dynamic_value(src):
        # Canonical-spec amendment ruling 1: a dynamic bind source anchored
        # at a runner workspace root is allowed with no manifest row; every
        # other dynamic source stays dynamic_route.
        if not workspace_anchored(src):
            findings.append(Finding("dynamic_route", "unresolved_value", boundary.path, line, text))
        return
    if is_host_path(src):
        findings.append(Finding("host_input_bind", "bind_source", boundary.path, line, text))


def check_mount_value(val, boundary, findings, line, text):
    if val is None:
        return
    if WAYLAND_ENV_BIND.search(val):
        findings.append(Finding("host_input_bind", "bind_source", boundary.path, line, text))
        return
    fields = {}
    for part in val.split(","):
        if "=" in part:
            k, v = part.split("=", 1)
            fields[k.strip()] = v.strip()
    src = fields.get("src") or fields.get("source")
    if src is None:
        return
    if is_dynamic_value(src):
        # Canonical-spec amendment ruling 1: a dynamic bind source anchored
        # at a runner workspace root is allowed with no manifest row; every
        # other dynamic source stays dynamic_route.
        if not workspace_anchored(src):
            findings.append(Finding("dynamic_route", "unresolved_value", boundary.path, line, text))
        return
    if is_host_path(src):
        findings.append(Finding("host_input_bind", "bind_source", boundary.path, line, text))


def check_display_assignment(name, raw_value, boundary, findings, line, text):
    """name=raw_value for DISPLAY/WAYLAND_DISPLAY/XAUTHORITY inside a boundary."""
    resolver = boundary.resolver
    ev = boundary.evidence
    if INHERITED_REF.search(raw_value):
        findings.append(Finding("host_display_projection", "display_inherited", boundary.path,
                                line, text))
        return
    resolved = resolver.resolve(raw_value)
    unresolved_ref = resolved is None and VAR_REF.search(raw_value) is not None
    if name == "DISPLAY":
        if resolved is not None and re.match(r"^:\d+$", resolved) \
                and resolved in ev.xvfb_displays and ev.cleanup_owned:
            return  # private Xvfb creation + literal display, same test boundary
        detail = "display_inherited" if unresolved_ref else "display_without_creation"
        findings.append(Finding("host_display_projection", detail,
                                boundary.path, line, text, excusable={"private_compositor"}))
    elif name == "WAYLAND_DISPLAY":
        if resolved is not None and resolved in ev.weston_sockets and ev.xdg_runtime_private:
            return  # private headless weston on a generated socket
        if resolved is None and ev.weston_headless and ev.xdg_runtime_private \
                and set(VAR_REF.findall(raw_value)) & ev.weston_socket_vars:
            return  # generated socket name variable shared with the weston creation
        detail = "display_inherited" if unresolved_ref else "display_without_creation"
        findings.append(Finding("host_display_projection", detail,
                                boundary.path, line, text, excusable={"private_compositor"}))
    else:  # XAUTHORITY
        findings.append(Finding("host_display_projection", "xauthority_assignment",
                                boundary.path, line, text))


def check_env_value(val, boundary, findings, line, text):
    if val is None:
        return
    if "=" not in val:
        if is_dynamic_value(val):
            findings.append(Finding("dynamic_route", "unresolved_env_name", boundary.path, line, text))
        elif val in DISPLAY_VARS:
            findings.append(Finding("host_display_projection", "display_inherited", boundary.path,
                                    line, text))
        return
    name, value = val.split("=", 1)
    if is_dynamic_value(name):
        findings.append(Finding("dynamic_route", "unresolved_env_name", boundary.path, line, text))
        return
    if name not in DISPLAY_VARS:
        return
    check_display_assignment(name, value, boundary, findings, line, text)


def match_container_tokens(toks, boundary, findings, start_line, text):
    """Match docker/podman-style flag values inside a token stream."""
    idx = 0
    n = len(toks)
    while idx < n:
        base = toks[idx].text.rsplit("/", 1)[-1]
        if base not in CONTAINER_PROGS:
            idx += 1
            continue
        idx += 1
        run_create = False
        has_image = False
        tainted = False
        # Global options may precede the subcommand (`docker --context ctx run
        # ...`). Known arities are consumed; an unknown global option makes the
        # command unparsable, so a following non-subcommand word is skipped as
        # its possible split value and the scan keeps looking for run/create
        # (#147 review round 2: an unknown split-value global option such as
        # `podman --root /tmp run "$IMAGE"` must still reach the subcommand).
        while idx < n:
            t = toks[idx].text
            if t == "container":
                # `docker container run|create|exec ...` spelling (#153).
                idx += 1
                continue
            if t in {"run", "create"}:
                run_create = True
                has_image = True
                idx += 1
                break
            if t == "exec":
                # exec has no image operand, but its -e/--env/--env-file
                # options project the same routes as run's (#153).
                run_create = True
                idx += 1
                break
            if t.startswith("-") and t != "--":
                flag = t.split("=", 1)[0]
                if "=" in t:
                    idx += 1
                elif flag in CONTAINER_GLOBAL_VALUE_FLAGS:
                    idx += 2
                elif flag in CONTAINER_GLOBAL_BOOL_FLAGS:
                    idx += 1
                else:
                    tainted = True
                    idx += 1
                    if (idx < n and not toks[idx].text.startswith("-")
                            and toks[idx].text not in {"run", "create", "exec", "container"}):
                        idx += 1
                continue
            break
        if (tainted and has_image
                and any(is_dynamic_value(tok.text) for tok in toks[idx:])):
            # Fail closed inside run/create only: a non-run/create subcommand
            # after an unknown global option has no image operand to prove
            # (`podman --root /tmp inspect "$CONTAINER"` is not a route; #147
            # review round 2 false positive). exec carries a container name,
            # not an image, so it is not an operand to prove either (#153).
            findings.append(Finding("dynamic_route", "dynamic_image_operand",
                                    boundary.path, start_line, text))
        seen_image = False
        after_delim = False
        while idx < n:
            tok = toks[idx]
            t = tok.text
            if t == "--" and not tainted:
                # End of options: the next positional is the image operand and
                # can no longer be reinterpreted as a flag (pf-hwprobe#23).
                # After an unknown-arity option the delimiter itself may be an
                # option value, so it is not trusted (#147 review).
                after_delim = True
                idx += 1
                continue
            if not t.startswith("-"):
                # A dynamic image operand is allowed only immediately after a
                # trusted `--`; assignment folding never proves its runtime
                # value. With a tainted parse no positional can be proven to be
                # the image, so scanning continues to the end of the command.
                # exec's first positional is a container name, not an image,
                # so the operand rule applies to run/create only (#153).
                if has_image and after_delim is False and is_dynamic_value(t):
                    findings.append(Finding("dynamic_route", "dynamic_image_operand",
                                            boundary.path, start_line, text))
                if after_delim:
                    seen_image = True
                    idx += 1
                    break
                if run_create and tainted:
                    idx += 1
                    continue
                seen_image = True
                idx += 1
                break
            if after_delim:
                # `--` followed by an option-shaped word is still red.
                findings.append(Finding("dynamic_route", "option_after_end_of_options",
                                        boundary.path, start_line, text))
                idx += 1
                continue
            flag = None
            inline = None
            if t == "-":
                idx += 1
                continue
            if t.startswith("--") or not run_create:
                flag = t.split("=", 1)[0]
                inline = t.split("=", 1)[1] if "=" in t else None
            else:
                # Decompose short-option tokens structurally (#153): inside
                # run/create/exec a short token is a cluster of boolean chars
                # that may end in a value char with an attached value
                # (-it, -eDISPLAY=$DISPLAY, -v/dev/input:/x, -itv/dev/input:/x).
                # A char that is neither a known boolean nor a known value
                # short makes the token undecomposable: it could hide an
                # attached -e/-v route that no taint can catch, because the
                # value is inside this token rather than a later operand, so
                # it fails closed instead of being skipped.
                chars = t[1:]
                j = 0
                undecomposable = False
                while j < len(chars):
                    ch = chars[j]
                    if ch in RUN_SHORT_BOOL_CHARS:
                        j += 1
                        continue
                    if ch in SHORT_VALUE_CHARS:
                        flag = "-" + ch
                        rest = chars[j + 1:]
                        if rest.startswith("="):
                            rest = rest[1:]
                        inline = rest or None
                        break
                    undecomposable = True
                    break
                if undecomposable:
                    findings.append(Finding("dynamic_route", "undecomposable_option",
                                            boundary.path, start_line, text))
                    tainted = True
                    idx += 1
                    continue
                if flag is None:
                    # Pure boolean cluster such as -it or -dit.
                    idx += 1
                    continue

            def value(max_tokens=1):
                nonlocal idx, inline
                if inline is not None:
                    idx += 1
                    return inline
                parts = []
                idx += 1
                while idx < n and len(parts) < max_tokens and not toks[idx].text.startswith("-"):
                    parts.append(toks[idx].text)
                    idx += 1
                return " ".join(parts) if parts else None

            if flag == "--device":
                check_device_value(value(), boundary, findings, start_line, text)
            elif flag in {"-v", "--volume"}:
                check_volume_value(value(), boundary, findings, start_line, text)
            elif flag == "--mount":
                check_mount_value(value(), boundary, findings, start_line, text)
            elif flag == "--env-file":
                # Fail closed on any --env-file (#153): the file can carry
                # DISPLAY/WAYLAND_DISPLAY/XAUTHORITY, and a literal repo path
                # does not make it green — it needs a manifest row like any
                # other route.
                findings.append(Finding("host_display_projection", "env_file",
                                        boundary.path, start_line, text))
                value()
            elif flag in {"-e", "--env"}:
                check_env_value(value(), boundary, findings, start_line, text)
            elif flag == "--device-cgroup-rule":
                val = value(max_tokens=3)
                if val is None:
                    continue
                if is_dynamic_value(val):
                    findings.append(Finding("dynamic_route", "unresolved_value", boundary.path,
                                            start_line, text))
                elif MAJOR13.match(val.strip()):
                    findings.append(Finding("input_major_wildcard", "cgroup_major13", boundary.path,
                                            start_line, text))
            elif run_create and flag in RUN_BOOL_FLAGS:
                idx += 1
            elif run_create and flag in RUN_VALUE_FLAGS:
                value()
            elif run_create:
                # Unknown option: a text scan cannot prove its arity, so the
                # rest of the command is parsed untrusted and any dynamic
                # operand fails closed (#147 review). The former
                # `inline is not None` catch-all silently consumed unknown
                # `--opt=value` forms; they now taint exactly like their split
                # spelling, so `run --mystery=x -- "$IMAGE"` stays red (#153).
                tainted = True
                idx += 1
            elif flag in SKIP_VALUE_FLAGS:
                value()
            else:
                idx += 1
        idx += 1


def match_permission_command(prog, arg_texts, boundary, findings, line, text):
    if prog in {"modprobe", "insmod"}:
        for value in arg_texts:
            base = value.rsplit("/", 1)[-1]
            if base in {"uinput", "uinput.ko", "uinput.ko.zst", "uinput.ko.xz"}:
                findings.append(Finding("host_uinput_permission", "permission_op", boundary.path,
                                        line, text))
        return
    if prog in {"sg", "newgrp"}:
        if arg_texts and arg_texts[0] == "input":
            findings.append(Finding("host_uinput_permission", "permission_op", boundary.path,
                                    line, text))
        return
    for value in arg_texts:
        if DEV_UINPUT.match(value) or DEV_INPUT.match(value):
            findings.append(Finding("host_uinput_permission", "permission_op", boundary.path,
                                    line, text))
            return


def match_bare_host_path(token_text, boundary, findings, line, text,
                         route_source=None, route_destination=None):
    for pattern, detail in HOST_PATHS:
        if pattern.search(token_text):
            excusable = {"product_session_capability"} if detail == "x11_socket_path" else frozenset()
            findings.append(Finding(
                "host_display_projection", detail, boundary.path, line, text,
                excusable=excusable, route_source=route_source,
                route_destination=route_destination,
            ))
            return


ASSIGN_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


OPERATOR_TEXTS = {"(", ")", "{", "}", "<", ">", "<<", ">>", "2>", "2>>", "<&", ">&", ";;"}


def _command_words(toks):
    return [t.text for t in toks if t.text not in OPERATOR_TEXTS
            and not re.match(r"^[<>]", t.text)]


def collect_command_evidence(toks, boundary):
    """Pass 1: gather private-compositor creation evidence for a boundary."""
    words = _command_words(toks)
    if not words:
        return
    idx = 0
    while idx < len(words) and words[idx] in {"export", "local", "declare", "typeset"}:
        idx += 1
    for word in words[idx:]:
        if "=" in word:
            name, value = word.split("=", 1)
            if ASSIGN_NAME.match(name):
                boundary.resolver.record(name, value)
                if name == "XDG_RUNTIME_DIR" and boundary.resolver.is_generated(value):
                    boundary.evidence.xdg_runtime_private = True
    prog = words[idx].rsplit("/", 1)[-1] if idx < len(words) else ""
    if prog in {"trap", "kill"}:
        boundary.evidence.cleanup_owned = True
    if prog == "xvfb-run":
        boundary.evidence.xvfb_run = True
    if prog == "Xvfb":
        for value in words[idx + 1:]:
            if re.match(r"^:\d+$", value):
                boundary.evidence.xvfb_displays.add(value)
    if prog in {"weston", "weston-headless"}:
        headless = any(w.startswith("--backend=headless") for w in words[idx + 1:])
        socket_val = None
        for word in words[idx + 1:]:
            if word.startswith("--socket="):
                socket_val = word.split("=", 1)[1]
            elif word == "--socket":
                socket_val = None
        if "--socket" in words[idx + 1:] and socket_val is None:
            pos = words.index("--socket", idx)
            if pos + 1 < len(words):
                socket_val = words[pos + 1]
        if headless and socket_val is not None:
            resolved = boundary.resolver.resolve(socket_val)
            if resolved is not None:
                boundary.evidence.weston_sockets.add(resolved)
            elif boundary.resolver.is_generated(socket_val):
                boundary.evidence.weston_socket_vars.update(VAR_REF.findall(socket_val))
                boundary.evidence.weston_headless = True


def env_assignment_tokens(args):
    """Return the DISPLAY-related assignments in a coreutils env prefix.

    Parses every option spelling coreutils accepts (#150 review round 2):
    `--` ends OPTION parsing only (GNU env runs `env -- DISPLAY=x cmd` with
    DISPLAY set, so the assignment loop still runs after it, #150
    verification), a bare `-` is the
    -i alias, short-option clusters whose final u/C consumes the rest of
    the cluster or the following operand, and -S/--split-string whose
    operand is itself re-parsed as options and assignments. After the
    options, NAME=VALUE tokens are collected until the first
    non-assignment word (the utility); only DISPLAY, WAYLAND_DISPLAY and
    XAUTHORITY assignments are returned for checking.
    """
    toks = list(args)
    i = 0
    while i < len(toks):
        t = toks[i]
        if t == "--":
            # GNU env: `--` ends OPTION parsing, not assignment parsing:
            # `env -- DISPLAY=x cmd` sets DISPLAY (#150 verification).
            del toks[i]
            break
        if t == "-":
            i += 1
            continue
        if not t.startswith("-"):
            break
        if t.startswith("--"):
            flag, inline = t.split("=", 1) if "=" in t else (t, None)
            if flag == "--split-string":
                operand = inline if inline is not None else (
                    toks[i + 1] if i + 1 < len(toks) else "")
                del toks[i:i + (1 if inline is not None else 2)]
                try:
                    toks[i:i] = shlex.split(operand)
                except ValueError:
                    toks[i:i] = operand.split()
                continue
            if flag in {"--unset", "--chdir"} and inline is None:
                i += 2
            else:
                i += 1
            continue
        cluster = t[1:]
        ci = 0
        consumed = False
        while ci < len(cluster):
            c = cluster[ci]
            rest = cluster[ci + 1:]
            if c == "S":
                operand = rest if rest else (
                    toks[i + 1] if i + 1 < len(toks) else "")
                del toks[i:i + (1 if rest else 2)]
                try:
                    toks[i:i] = shlex.split(operand)
                except ValueError:
                    toks[i:i] = operand.split()
                consumed = True
                break
            if c in "uC":
                del toks[i:i + (1 if rest else 2)]
                consumed = True
                break
            ci += 1
        if not consumed:
            del toks[i]  # pure boolean cluster such as -i or -iv
    out = []
    while i < len(toks):
        t = toks[i]
        if "=" not in t:
            break  # utility reached; the rest are its args
        name = t.split("=", 1)[0]
        if not ASSIGN_NAME.match(name):
            break
        if name in DISPLAY_VARS:
            out.append(t)
        i += 1
    return out


def match_shell_commands(commands, boundary, findings, text_of, heredocs=()):
    """Pass 2: flag host routes inside parsed shell commands."""
    for start_line, toks in commands:
        collect_command_evidence(toks, boundary)
    for start_line, toks in commands:
        text = text_of(start_line)
        match_container_tokens(toks, boundary, findings, start_line, text)
        words = _command_words(toks)
        if not words:
            continue
        idx = 0
        while idx < len(words) and words[idx] in {"export", "local", "declare", "typeset"}:
            idx += 1
        # Environment-prefix assignments: DISPLAY=:0 cmd ...
        while idx < len(words) and "=" in words[idx]:
            name, value = words[idx].split("=", 1)
            if not ASSIGN_NAME.match(name):
                break
            if name in DISPLAY_VARS:
                check_display_assignment(name, value, boundary, findings, start_line, text)
            idx += 1
        if idx >= len(words):
            continue
        prog = words[idx].rsplit("/", 1)[-1]
        args = words[idx + 1:]
        if prog == "env":
            # Canonical-spec amendment ruling 2, parsed with the full
            # coreutils option grammar (#150 review round 2): env running a
            # host process is not a projection. env_assignment_tokens
            # parses clusters, the bare `-` alias and -S/--split-string
            # operands and returns only the DISPLAY/WAYLAND_DISPLAY/
            # XAUTHORITY assignments that precede the utility; tokens
            # after the utility or after `--` are arguments. Container env
            # options are handled by match_container_tokens/compose.
            for value in env_assignment_tokens(args):
                name, val2 = value.split("=", 1)
                check_display_assignment(name, val2, boundary, findings,
                                         start_line, text)
            continue
        if prog in DISPLAY_TOOLS:
            findings.append(Finding("host_display_projection", "display_tool", boundary.path,
                                    start_line, text))
            continue
        if prog in CLIPBOARD_TOOLS:
            findings.append(Finding("host_clipboard_write", "clipboard_tool", boundary.path,
                                    start_line, text))
            continue
        if prog in PERMISSION_PROGS | {"modprobe", "insmod", "sg", "newgrp"}:
            match_permission_command(prog, args, boundary, findings, start_line, text)
        for value in words:
            match_bare_host_path(value, boundary, findings, start_line, text)
    for start_line, delim, body in heredocs:
        text = text_of(start_line)
        scan_embedded_body(body, start_line, delim, boundary, findings, text_of)


def scan_embedded_body(body, base_line, delim, boundary, findings, text_of):
    hint = delim.lower()
    shell_like = any(marker in hint for marker in ("sh", "bash", "eof", "script"))
    if shell_like:
        try:
            commands, heredocs = lex_shell_text(body, base_line)
        except MalformedFile as exc:
            findings.append(Finding("dynamic_route", "malformed_file", boundary.path,
                                    base_line, str(exc)))
            return
        match_shell_commands(commands, boundary, findings, text_of, heredocs)
    else:
        scan_source_text(body, base_line, boundary, findings)


def match_loose_tokens(toks, boundary, findings, line, text,
                       route_source=None, route_destination=None):
    """Non-shell contexts: flag obvious host-route tokens on a logical line."""
    match_container_tokens(toks, boundary, findings, line, text)
    words = _command_words(toks)
    for pos, word in enumerate(words):
        base = word.rsplit("/", 1)[-1]
        if base in DISPLAY_TOOLS:
            findings.append(Finding("host_display_projection", "display_tool", boundary.path,
                                    line, text))
        elif base in CLIPBOARD_TOOLS:
            findings.append(Finding("host_clipboard_write", "clipboard_tool", boundary.path,
                                    line, text))
        elif base in {"modprobe", "insmod"}:
            for value in words[pos + 1:]:
                if value.rsplit("/", 1)[-1].split(".")[0] == "uinput":
                    findings.append(Finding("host_uinput_permission", "permission_op",
                                            boundary.path, line, text))
                    break
        elif base in {"sg", "newgrp"}:
            if pos + 1 < len(words) and words[pos + 1] == "input":
                findings.append(Finding("host_uinput_permission", "permission_op",
                                        boundary.path, line, text))
        elif base in PERMISSION_PROGS:
            if any(v.startswith("/dev/") for v in words[pos + 1:]):
                match_permission_command(base, words[pos + 1:], boundary, findings, line, text)
        elif word == "--device-cgroup-rule":
            tail = " ".join(words[pos + 1:pos + 4]).strip("\"'=")
            if MAJOR13.match(tail):
                findings.append(Finding("input_major_wildcard", "cgroup_major13", boundary.path,
                                        line, text))
        elif word.startswith("--device-cgroup-rule="):
            tail = word.split("=", 1)[1].strip("\"'")
            if MAJOR13.match(tail):
                findings.append(Finding("input_major_wildcard", "cgroup_major13", boundary.path,
                                        line, text))
        elif "=" in word and word.split("=", 1)[0].endswith("--device"):
            check_device_value(word.split("=", 1)[1], boundary, findings, line, text,
                               container_ctx=False)
        elif word == "--device" and pos + 1 < len(words):
            check_device_value(words[pos + 1], boundary, findings, line, text,
                               container_ctx=False)
        else:
            match_bare_host_path(
                word, boundary, findings, line, text,
                route_source=route_source, route_destination=route_destination,
            )


def scan_source_text(text, base_line, boundary, findings, shell_commands=False):
    """Loose scan for source files: strings, bare tokens, clipboard APIs."""
    for line_no, logical in join_logical_lines(text.splitlines(), base_line):
        stripped = logical.strip()
        if not stripped:
            continue
        literal_matches = list(STRING_LITERAL.finditer(stripped))
        route_by_destination_span = {}
        for route in SOURCE_ROUTE_PAIR.finditer(stripped):
            destination_pos = route.start("destination")
            for literal_match in literal_matches:
                if literal_match.start() < destination_pos < literal_match.end():
                    route_by_destination_span[literal_match.span()] = (
                        route.group("source"), route.group("destination")
                    )
                    break
        for literal_match in literal_matches:
            literal = (literal_match.group(1) if literal_match.group(1) is not None
                       else literal_match.group(2))
            route_endpoints = route_by_destination_span.get(literal_match.span(), (None, None))
            for tok in lex_tolerant(literal, line_no):
                match_loose_tokens(
                    [tok], boundary, findings, line_no, stripped,
                    route_source=route_endpoints[0], route_destination=route_endpoints[1],
                )
        toks = lex_tolerant(STRING_LITERAL.sub(" ", stripped), line_no)
        match_loose_tokens(toks, boundary, findings, line_no, stripped)
        if CLIPBOARD_API.search(stripped):
            findings.append(Finding("host_clipboard_write", "clipboard_api", boundary.path,
                                    line_no, stripped))
        for lib in CLIPBOARD_LIBS:
            if re.search(r"[\b\"'/]" + re.escape(lib) + r"[\b\"']", stripped) or \
                    stripped.startswith(lib):
                findings.append(Finding("host_clipboard_write", "clipboard_lib", boundary.path,
                                        line_no, stripped))
                break
        m = SOURCE_SETENV.search(stripped)
        if m:
            findings.append(Finding("host_display_projection", "set_var_display", boundary.path,
                                    line_no, stripped))
        if shell_commands:
            try:
                commands, heredocs = lex_shell_text(stripped, line_no)
            except MalformedFile:
                continue
            match_shell_commands(commands, boundary, findings,
                                 lambda ln: stripped, heredocs)


def scan_systemd(text, base_line, boundary, findings):
    for line_no, logical in join_logical_lines(text.splitlines(), base_line):
        stripped = logical.strip()
        if not stripped or stripped.startswith(("#", ";", "[")):
            continue
        if "modules-load" in boundary.path:
            if stripped == "uinput":
                findings.append(Finding("host_uinput_permission", "modules_load", boundary.path,
                                        line_no, stripped))
            continue
        if "=" not in stripped:
            continue
        key, value = stripped.split("=", 1)
        key = key.strip()
        value = value.strip()
        if key == "DeviceAllow":
            fields = value.split()
            if fields and (DEV_UINPUT.match(fields[0]) or DEV_INPUT.match(fields[0])):
                findings.append(Finding("host_uinput_permission", "device_allow", boundary.path,
                                        line_no, stripped))
        elif key in {"BindPaths", "BindReadOnlyPaths"}:
            for entry in value.split():
                src = entry.split(":")[0]
                if is_dynamic_value(src):
                    findings.append(Finding("dynamic_route", "unresolved_value", boundary.path,
                                            line_no, stripped))
                elif is_host_path(src):
                    for pattern, detail in HOST_PATHS:
                        if pattern.search(src) and detail == "x11_socket_path":
                            findings.append(Finding("host_display_projection", detail,
                                                    boundary.path, line_no, stripped,
                                                    excusable={"product_session_capability"}))
                            break
                    else:
                        findings.append(Finding("host_input_bind", "bind_source", boundary.path,
                                                line_no, stripped))
        elif key == "Environment":
            try:
                parts = shlex.split(value, posix=True)
            except ValueError:
                parts = value.split()
            for item in parts:
                check_env_value(item, boundary, findings, line_no, stripped)
        elif key.startswith("Exec"):
            toks = lex_tolerant(value, line_no)
            words = _command_words(toks)
            prog = words[0].lstrip("@-:+!").rsplit("/", 1)[-1] if words else ""
            match_container_tokens(toks, boundary, findings, line_no, stripped)
            if prog in DISPLAY_TOOLS:
                findings.append(Finding("host_display_projection", "display_tool", boundary.path,
                                        line_no, stripped))
            if prog in CLIPBOARD_TOOLS:
                findings.append(Finding("host_clipboard_write", "clipboard_tool", boundary.path,
                                        line_no, stripped))
            if prog in PERMISSION_PROGS | {"modprobe", "insmod", "sg", "newgrp"}:
                match_permission_command(prog, words[1:], boundary, findings, line_no, stripped)
            for word in words:
                match_bare_host_path(word, boundary, findings, line_no, stripped)


def _walk_yaml(node, visitor, path_stack=()):
    if not isinstance(node, YNode):
        return
    value = node.value
    if isinstance(value, dict):
        for key, child in value.items():
            visitor(key, child, path_stack + (str(key),))
            _walk_yaml(child, visitor, path_stack + (str(key),))
    elif isinstance(value, list):
        for child in value:
            _walk_yaml(child, visitor, path_stack)


def scan_yaml_generic(tree, boundary, findings):
    resolver = boundary.resolver

    def visitor(key, node, path_stack):
        key_name = str(key)
        if not isinstance(node, YNode):
            return
        value = node.value
        if key_name == "environment" and isinstance(value, dict):
            for name, child in value.items():
                if str(name) in DISPLAY_VARS and isinstance(child, YNode) \
                        and child.value is not None:
                    check_display_assignment(str(name), str(child.value), boundary, findings,
                                             child.line, boundary.line_text(child.line))
            return
        if not isinstance(value, list):
            return
        items = [i for i in value if isinstance(i, YNode)]
        if key_name == "devices":
            for item in items:
                val = str(item.value) if item.value is not None else None
                text = boundary.line_text(item.line)
                if val is None:
                    continue
                if is_dynamic_value(val):
                    findings.append(Finding("dynamic_route", "unresolved_value", boundary.path,
                                            item.line, text))
                elif DEV_UINPUT.match(val) or DEV_INPUT.match(val):
                    findings.append(Finding("host_uinput_device", "compose_device",
                                            boundary.path, item.line, text))
        elif key_name in {"volumes", "volume"}:
            for item in items:
                if item.value is not None:
                    check_volume_value(str(item.value), boundary, findings, item.line,
                                       boundary.line_text(item.line))
        elif key_name == "environment":
            for item in items:
                if item.value is not None:
                    check_env_value(str(item.value), boundary, findings, item.line,
                                    boundary.line_text(item.line))

    _walk_yaml(tree, visitor)


# --- Runner-label host map + runs-on evaluator ------------------------------

MAP_SEAT_POLICIES = ("forbidden", "present", "unknown")
MAP_HOST_CLASSES = ("github-hosted", "vm", "bare-metal")
MAP_ENTRY_REQUIRED = ("runner", "host_class", "ephemeral", "seat_policy",
                      "provisioning_sources", "may_launch_pfvd_guest")


class JobAnalysis:
    def __init__(self, job_id, line):
        self.job_id = job_id
        self.line = line
        self.end_line = 10 ** 9
        self.label_sets = []
        self.dynamic = False


def load_host_map(root, failures):
    """Load and validate the vendored runner-host map plus its .SOURCE pin."""
    path = root / MAP_RELPATH
    if not path.exists():
        failures.append(Finding("dynamic_route", "missing_host_map", MAP_RELPATH, 1,
                                "input-runner-host-map-v1.json is not vendored"))
        return None
    raw = path.read_bytes()
    try:
        doc = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as exc:
        failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, 1, str(exc)))
        return None
    if doc.get("schema_version") != 1:
        failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, 1,
                                "schema_version must be 1"))
        return None
    if not doc.get("map_id"):
        failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, 1,
                                "map_id is required"))
        return None
    entries = doc.get("entries")
    if not isinstance(entries, list) or not entries:
        failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, 1,
                                "entries must be a non-empty list"))
        return None
    seen_runners = set()
    for offset, entry in enumerate(entries):
        line = offset + 1
        if not isinstance(entry, dict):
            failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                    "entry must be an object"))
            return None
        for field in MAP_ENTRY_REQUIRED:
            if field not in entry:
                failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                        f"entry missing required field {field}"))
                return None
        if "labels" not in entry and "label_patterns" not in entry:
            failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                    "entry needs labels or label_patterns"))
            return None
        for field in ("labels", "label_patterns", "provisioning_sources"):
            value = entry.get(field)
            if value is not None and (not isinstance(value, list) or not value
                                      or not all(isinstance(v, str) and v for v in value)):
                failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                        f"{field} must be a non-empty list of strings"))
                return None
        if entry.get("host_class") not in MAP_HOST_CLASSES:
            failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                    f"host_class must be one of {MAP_HOST_CLASSES}"))
            return None
        if entry.get("seat_policy") not in MAP_SEAT_POLICIES:
            failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                    f"seat_policy must be one of {MAP_SEAT_POLICIES}"))
            return None
        for field in ("ephemeral", "may_launch_pfvd_guest"):
            if not isinstance(entry.get(field), bool):
                failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                        f"{field} must be a boolean"))
                return None
        if entry["runner"] in seen_runners:
            failures.append(Finding("dynamic_route", "malformed_host_map", MAP_RELPATH, line,
                                    f"duplicate runner id {entry['runner']}"))
            return None
        seen_runners.add(entry["runner"])
    schema_path = root / MAP_SCHEMA_RELPATH
    if schema_path.exists():
        try:
            schema = json.loads(schema_path.read_text(encoding="utf-8"))
            props = schema.get("definitions", {}).get("entry", {}).get("properties", {})
            required = schema.get("definitions", {}).get("entry", {}).get("required", [])
            seat_enum = props.get("seat_policy", {}).get("enum", [])
            if set(required) != set(MAP_ENTRY_REQUIRED) or set(seat_enum) != set(MAP_SEAT_POLICIES):
                failures.append(Finding("-", "policy_mismatch", MAP_SCHEMA_RELPATH, 1,
                                        "schema diverges from the canonical entry contract"))
        except ValueError as exc:
            failures.append(Finding("dynamic_route", "malformed_host_map", MAP_SCHEMA_RELPATH, 1,
                                    str(exc)))
    source_path = root / MAP_SOURCE_RELPATH
    if source_path.exists():
        meta = {}
        for meta_line in source_path.read_text(encoding="utf-8").splitlines():
            meta_line = meta_line.strip()
            if not meta_line or meta_line.startswith("#") or "=" not in meta_line:
                continue
            k, v = meta_line.split("=", 1)
            meta[k.strip()] = v.strip()
        for field in ("source_commit", "source_path", "fixture_sha256"):
            if not meta.get(field):
                failures.append(Finding("dynamic_route", "malformed_host_map",
                                        MAP_SOURCE_RELPATH, 1,
                                        f".SOURCE missing {field}"))
                return None
        digest = hashlib.sha256(raw).hexdigest()
        if meta["fixture_sha256"] != digest:
            failures.append(Finding("dynamic_route", "source_sha_mismatch", MAP_SOURCE_RELPATH, 1,
                                    f"map sha256 {digest} != pinned {meta['fixture_sha256']}"))
    return doc


def entry_matches_labels(entry, labels):
    lowered = [l.lower() for l in labels]
    entry_labels = [l.lower() for l in entry.get("labels", [])]
    if entry_labels:
        return set(lowered) <= set(entry_labels)
    patterns = entry.get("label_patterns", [])
    if patterns:
        return all(any(fnmatch.fnmatch(l, p) for p in patterns) for l in lowered)
    return False


def eligible_hosts(host_map, labels):
    if host_map is None:
        return []
    return [e for e in host_map["entries"] if entry_matches_labels(e, labels)]


def host_descriptor(entry):
    return f"{entry['runner']}({entry['host_class']},seat={entry['seat_policy']})"


def resolve_runs_on(node, matrix_node, failures, path, line):
    """GitHub-subset runs-on resolution. Returns a list of label sets, or
    None when the value is dynamic (fail closed)."""
    if node is None or node.value is None:
        failures.append(Finding("dynamic_route", "dynamic_runs_on", path, line,
                                "runs-on is missing or empty"))
        return None
    value = node.value
    if isinstance(value, str):
        items = [value]
    elif isinstance(value, list):
        items = [str(i.value) if isinstance(i, YNode) else str(i) for i in value]
    else:
        items = [str(value)]
    label_sets = [[]]
    for item in items:
        expr = GITHUB_EXPR.fullmatch(item.strip())
        if expr:
            inner = expr.group(1).strip()
            m = re.fullmatch(r"matrix\.([A-Za-z_][A-Za-z0-9_.\-]*)", inner)
            if m and isinstance(matrix_node, YNode) and isinstance(matrix_node.value, dict):
                axis = None
                for key, child in matrix_node.value.items():
                    if str(key) == m.group(1):
                        axis = child
                if isinstance(axis, YNode) and isinstance(axis.value, list):
                    options = [str(o.value) if isinstance(o, YNode) else str(o)
                               for o in axis.value]
                    if all(not GITHUB_EXPR.search(o) for o in options):
                        label_sets = [prev + [o] for prev in label_sets for o in options]
                        continue
            failures.append(Finding("dynamic_route", "dynamic_runs_on", path, line,
                                    f"runs-on expression {item!r} is not statically resolvable"))
            return None
        label_sets = [prev + [item] for prev in label_sets]
    return label_sets


def analyze_workflow(rel, text, findings, jobs_out, contract):
    """Parse one workflow file, extract per-job runs-on label sets and scan
    job steps for host routes."""
    try:
        tree = parse_yaml_subset(text)
    except MalformedFile as exc:
        contract.append(Finding("dynamic_route", "malformed_file", rel, 1, str(exc)))
        return
    boundary_all = Boundary(rel, "workflow", text=text)
    lines = text.splitlines()

    def text_of(line_no):
        if 0 < line_no <= len(lines):
            return lines[line_no - 1].strip()
        return ""

    jobs_node = tree.get("jobs") if isinstance(tree.value, dict) else None
    if not isinstance(jobs_node, YNode) or not isinstance(jobs_node.value, dict):
        return
    job_ids = list(jobs_node.value.keys())
    for index, job_id in enumerate(job_ids):
        job = jobs_node.value[job_id]
        line = jobs_node.key_lines.get(job_id, jobs_node.line)
        analysis = JobAnalysis(job_id, line)
        analysis.end_line = (jobs_node.key_lines.get(job_ids[index + 1], jobs_node.line) - 1
                             if index + 1 < len(job_ids) else 10 ** 9)
        jobs_out.append(analysis)
        if not isinstance(job.value, dict):
            continue
        uses = job.get("uses")
        runs_on_node = job.get("runs-on")
        if isinstance(uses, YNode) and uses.value is not None and runs_on_node is None:
            continue  # reusable workflow call: labels belong to the callee file
        matrix_node = None
        strategy = job.get("strategy")
        if isinstance(strategy, YNode) and isinstance(strategy.value, dict):
            matrix_node = strategy.value.get("matrix")
        label_sets = resolve_runs_on(runs_on_node, matrix_node, contract, rel, line)
        if label_sets is None:
            analysis.dynamic = True
        else:
            analysis.label_sets = label_sets
        resolver = Resolver()
        for scope in (job,):
            env = scope.get("env") if isinstance(scope.value, dict) else None
            if isinstance(env, YNode) and isinstance(env.value, dict):
                for name, child in env.value.items():
                    if isinstance(child, YNode) and child.value is not None \
                            and not GITHUB_EXPR.search(str(child.value)):
                        resolver.record(str(name), str(child.value))
        job_boundary = Boundary(rel, "workflow", resolver=resolver, text=text)
        job_env = job.get("env")
        if isinstance(job_env, YNode) and isinstance(job_env.value, dict):
            for name, child in job_env.value.items():
                if str(name) in DISPLAY_VARS and isinstance(child, YNode) \
                        and child.value is not None:
                    value = str(child.value)
                    if GITHUB_EXPR.search(value):
                        findings.append(Finding("host_display_projection", "display_inherited",
                                                rel, child.line, text_of(child.line)))
                    else:
                        check_display_assignment(str(name), value, job_boundary, findings,
                                                 child.line, text_of(child.line))
        job_container = job.get("container")
        if isinstance(job_container, YNode):
            _scan_step_container(job_container, job_boundary, findings, text_of)
        services = job.get("services")
        if isinstance(services, YNode) and isinstance(services.value, dict):
            for service in services.value.values():
                if isinstance(service, YNode):
                    _scan_step_container(service, job_boundary, findings, text_of)
        steps = job.get("steps")
        if isinstance(steps, YNode) and isinstance(steps.value, list):
            for step in steps.value:
                if not isinstance(step, YNode) or not isinstance(step.value, dict):
                    continue
                step_env = step.value.get("env")
                if isinstance(step_env, YNode) and isinstance(step_env.value, dict):
                    for name, child in step_env.value.items():
                        if isinstance(child, YNode) and child.value is not None \
                                and not GITHUB_EXPR.search(str(child.value)):
                            resolver.record(str(name), str(child.value))
                boundary = Boundary(rel, "workflow", resolver=resolver, text=text)
                env_direct = None
                if isinstance(step_env, YNode) and isinstance(step_env.value, dict):
                    env_direct = step_env.value
                if env_direct:
                    for name, child in env_direct.items():
                        if str(name) in DISPLAY_VARS and isinstance(child, YNode) \
                                and child.value is not None:
                            value = str(child.value)
                            if GITHUB_EXPR.search(value):
                                findings.append(Finding("host_display_projection",
                                                        "display_inherited", rel, child.line,
                                                        text_of(child.line)))
                            else:
                                check_display_assignment(str(name), value, boundary, findings,
                                                         child.line, text_of(child.line))
                container = step.value.get("container")
                if isinstance(container, YNode):
                    _scan_step_container(container, boundary, findings, text_of)
                run = step.value.get("run")
                if isinstance(run, YNode) and isinstance(run.value, str):
                    block_line = run.body_line or run.line
                    try:
                        commands, heredocs = lex_shell_text(run.value, block_line)
                    except MalformedFile as exc:
                        contract.append(Finding("dynamic_route", "malformed_file", rel,
                                                block_line, str(exc)))
                        continue
                    boundary.evidence = Evidence()
                    match_shell_commands(commands, boundary, findings, text_of, heredocs)
    wf_env = tree.get("env") if isinstance(tree.value, dict) else None
    if isinstance(wf_env, YNode) and isinstance(wf_env.value, dict):
        for name, child in wf_env.value.items():
            if str(name) in DISPLAY_VARS and isinstance(child, YNode) and child.value is not None:
                value = str(child.value)
                line = child.line
                if GITHUB_EXPR.search(value):
                    findings.append(Finding("host_display_projection", "display_inherited",
                                            rel, line, text_of(line)))
                else:
                    check_display_assignment(str(name), value, Boundary(rel, "workflow", text=text),
                                             findings, line, text_of(line))


def _scan_step_container(container, boundary, findings, text_of):
    value = container.value
    if isinstance(value, str):
        toks = [Tok("docker", container.line)] + lex_tolerant(value, container.line)
        match_container_tokens(toks, boundary, findings, container.line,
                               text_of(container.line))
        return
    if not isinstance(value, dict):
        return
    image = value.get("image")
    options = value.get("options")
    if isinstance(options, YNode) and isinstance(options.value, str):
        toks = [Tok("docker", options.line)] + lex_tolerant(options.value, options.line)
        match_container_tokens(toks, boundary, findings, options.line, text_of(options.line))
    volumes = value.get("volumes")
    if isinstance(volumes, YNode) and isinstance(volumes.value, list):
        for item in volumes.value:
            if isinstance(item, YNode) and item.value is not None:
                check_volume_value(str(item.value), boundary, findings, item.line,
                                   text_of(item.line))
    env = value.get("env")
    if isinstance(env, YNode) and isinstance(env.value, dict):
        for name, child in env.value.items():
            if str(name) in DISPLAY_VARS and isinstance(child, YNode) and child.value is not None:
                value_s = str(child.value)
                if GITHUB_EXPR.search(value_s):
                    findings.append(Finding("host_display_projection", "display_inherited",
                                            boundary.path, child.line, text_of(child.line)))
                else:
                    check_display_assignment(str(name), value_s, boundary, findings,
                                             child.line, text_of(child.line))


def attribute_jobs(findings, jobs, path):
    for finding in findings:
        if finding.path != path or finding.job is not None:
            continue
        for job in jobs:
            if job.line <= finding.line <= job.end_line:
                finding.job = job.job_id
                break


# --- Exceptions, manifest and driver -----------------------------------------

def apply_product_session_capability(findings, policy):
    """product_session_capability: the producer-owned X11 capability source is
    the product's own session plumbing. It only excuses the individual source
    occurrence that carries both route endpoints; never workflows, shell or units."""
    for finding in findings:
        if finding.excused or "product_session_capability" not in finding.excusable:
            continue
        if finding.kind != "source":
            continue
        if finding.route_source == policy.producer_x11_source \
                and finding.route_destination == policy.producer_x11_destination:
            finding.excused = True


def load_host_manifest(root, policy, failures, file_texts):
    """Parse host-route rows from the shared manifest. Returns allowed keys."""
    path = root / MANIFEST_RELPATH
    allowed = {}
    if not path.exists():
        return allowed
    for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if line.startswith("#") or HOST_ROUTE_TAG not in line:
            continue
        head, _, meta = line.partition("\t# host-route ")
        parts = head.split("\t")
        if len(parts) != 2 or not re.fullmatch(r"[0-9a-f]{64}", parts[1]):
            failures.append(Finding("dynamic_route", "malformed_inventory_row",
                                    MANIFEST_RELPATH, line_no, line.strip()))
            continue
        key = head
        rel_path = parts[0]
        if rel_path.startswith(".github/workflows/"):
            failures.append(Finding("dynamic_route", "invalid_exception", MANIFEST_RELPATH,
                                    line_no, f"{rel_path}: workflow occurrences cannot be "
                                    "classified by manifest rows"))
            continue
        fields = {}
        note_parts = []
        for token in meta.split():
            if "=" in token and not token.startswith(("note:", "#")):
                k, v = token.split("=", 1)
                fields[k] = v
            else:
                note_parts.append(token)
        rule = fields.get("rule")
        klass = fields.get("class")
        if rule not in policy.rules:
            failures.append(Finding("dynamic_route", "malformed_inventory_row",
                                    MANIFEST_RELPATH, line_no, f"unknown rule {rule!r}"))
            continue
        if klass not in policy.classes:
            failures.append(Finding("dynamic_route", "malformed_inventory_row",
                                    MANIFEST_RELPATH, line_no, f"unknown class {klass!r}"))
            continue
        invalid = False
        for required in policy.classes[klass]:
            if not fields.get(required):
                failures.append(Finding("dynamic_route", "invalid_exception", MANIFEST_RELPATH,
                                        line_no, f"class {klass} requires {required}="))
                invalid = True
        if invalid:
            continue
        if klass == "guest_internal":
            caller = fields.get("caller", "")
            caller_path = root / caller
            valid = caller and caller_path.exists() and \
                any(fnmatch.fnmatch(caller, pat) for pat in policy.guest_entry_points)
            if valid:
                caller_text = file_texts.get(caller)
                if caller_text is None:
                    try:
                        caller_text = caller_path.read_text(encoding="utf-8")
                    except (OSError, UnicodeDecodeError):
                        caller_text = ""
                valid = Path(rel_path).name in caller_text
            if not valid:
                failures.append(Finding("dynamic_route", "invalid_exception", MANIFEST_RELPATH,
                                        line_no, f"guest_internal caller {caller!r} is not a "
                                        "guest entry point that references the occurrence"))
                continue
        if klass == "product_clipboard_adapter":
            # The adapter's own definition file must carry the gate
            # marker. Call sites are classified instead as rows of class
            # product_clipboard_adapter_call: they are bound by the exact
            # occurrence line hash alone, so a new or changed call site is
            # unclassified and fails until a human classifies it in review
            # (#150 verification ruling; the adapter= field, its comment/
            # string stripper and its import-binding heuristic are deleted
            # - a text scanner cannot resolve call receivers).
            text = file_texts.get(rel_path, "")
            if policy.clipboard_gate_marker not in text:
                failures.append(Finding("dynamic_route", "invalid_exception", MANIFEST_RELPATH,
                                        line_no, f"{rel_path} lacks the "
                                        f"{policy.clipboard_gate_marker} gate marker"))
                continue
        if key in allowed:
            failures.append(Finding("dynamic_route", "duplicate_inventory_row",
                                    MANIFEST_RELPATH, line_no, f"duplicate row for {key}"))
            continue
        allowed[key] = (rule, klass, line_no)
    return allowed


def evaluate_runs_on(workflow_jobs, findings, host_map, failures, report_lines):
    """Fail when a host-input/display/clipboard consumer job can run on any
    committed host, and when runs-on is dynamic. Output always names the
    runner labels and every eligible committed host class."""
    consumer_rules = {"host_uinput_device", "host_input_bind", "input_major_wildcard",
                      "host_uinput_permission", "host_display_projection",
                      "host_clipboard_write", "dynamic_route"}
    for path, jobs in workflow_jobs.items():
        file_findings = [f for f in findings
                         if f.path == path and not f.excused and f.rule in consumer_rules]
        for job in jobs:
            job_findings = [f for f in file_findings
                            if f.job == job.job_id or (f.job is None
                                                       and job.line <= f.line <= job.end_line)]
            if job.dynamic:
                for label_set in [[]]:
                    report_lines.append(f"runs_on={path}#{job.job_id} labels=[dynamic] "
                                        "eligible=[unresolved]")
                continue
            if not job.label_sets:
                continue  # reusable-workflow call; the callee file carries the labels
            for labels in job.label_sets:
                eligible = eligible_hosts(host_map, labels)
                names = "[" + ",".join(labels) + "]"
                hosts = "[" + ",".join(host_descriptor(e) for e in eligible) + "]"
                report_lines.append(f"runs_on={path}#{job.job_id} labels={names} "
                                    f"eligible={hosts}")
                if not job_findings:
                    continue
                rules = sorted({f.rule for f in job_findings})
                if eligible:
                    failures.append(Finding("dynamic_route", "host_consumer_job", path,
                                            job.line, f"job {job.job_id} labels={names} "
                                            f"eligible={hosts} consumes host routes "
                                            f"rules={rules}"))
                else:
                    failures.append(Finding("dynamic_route", "no_eligible_host", path, job.line,
                                            f"job {job.job_id} labels={names} consumes host "
                                            f"routes rules={rules} but matches no committed "
                                            "host class"))


def scan_host_routes(root, use_manifest=True, quiet=False):
    """Full host-route pass. Returns (failures, report_lines, stats)."""
    failures = []
    report_lines = []
    policy = load_policy(root, failures)
    all_findings = []
    file_texts = {}
    workflow_jobs = {}
    files_scanned = 0
    for rel in tracked(root=root):
        path = root / rel
        try:
            head = path.open("r", encoding="utf-8", errors="strict").readline()
        except (OSError, UnicodeDecodeError):
            continue
        kind = host_route_file_kind(rel, head)
        if kind is None:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        files_scanned += 1
        file_texts[rel.as_posix()] = text
        boundary = Boundary(rel.as_posix(), kind, text=text)
        before = len(all_findings)
        if kind == "workflow":
            jobs = []
            analyze_workflow(rel.as_posix(), text, all_findings, jobs, failures)
            workflow_jobs[rel.as_posix()] = jobs
            scan_source_text(text, 1, boundary, all_findings, shell_commands=False)
        elif kind == "yaml":
            try:
                tree = parse_yaml_subset(text)
            except MalformedFile as exc:
                all_findings.append(Finding("dynamic_route", "malformed_file", rel.as_posix(),
                                            1, str(exc)))
                tree = None
            if tree is not None:
                scan_yaml_generic(tree, boundary, all_findings)
            scan_source_text(text, 1, boundary, all_findings)
        elif kind == "systemd":
            scan_systemd(text, 1, boundary, all_findings)
        elif kind == "shell":
            try:
                commands, heredocs = lex_shell_text(text, 1)
            except MalformedFile as exc:
                all_findings.append(Finding("dynamic_route", "malformed_file", rel.as_posix(),
                                            1, str(exc)))
                commands, heredocs = [], []
            lines = text.splitlines()

            def text_of(line_no, _lines=lines):
                if 0 < line_no <= len(_lines):
                    return _lines[line_no - 1].strip()
                return ""

            match_shell_commands(commands, boundary, all_findings, text_of, heredocs)
        else:
            scan_source_text(text, 1, boundary, all_findings)
        for finding in all_findings[before:]:
            finding.kind = kind
    for path, jobs in workflow_jobs.items():
        attribute_jobs(all_findings, jobs, path)
    apply_product_session_capability(all_findings, policy)
    dedup = {}
    for finding in all_findings:
        sig = (finding.path, finding.line, finding.rule, finding.detail)
        if sig not in dedup or (dedup[sig].excused and not finding.excused):
            dedup[sig] = finding
    findings = sorted(dedup.values(), key=lambda f: (f.path, f.line, f.rule))
    live = {}
    for finding in findings:
        if not finding.excused:
            live.setdefault(finding.key, finding)
    excused = sum(1 for f in findings if f.excused)
    if use_manifest:
        allowed = load_host_manifest(root, policy, failures, file_texts)
        for key, finding in sorted(live.items()):
            if key not in allowed:
                failures.append(Finding(finding.rule, "unclassified", finding.path, finding.line,
                                        finding.text))
        allowed_keys = set(allowed)
        live_keys = set(live)
        for key in sorted(allowed_keys - live_keys):
            failures.append(Finding("dynamic_route", "stale_inventory", MANIFEST_RELPATH, 0,
                                    f"manifest row no longer matches any live occurrence: "
                                    f"{key}"))
    host_map = load_host_map(root, failures)
    evaluate_runs_on(workflow_jobs, findings, host_map, failures, report_lines)
    stats = {"files": files_scanned,
             "findings_classified": sum(1 for k in live if use_manifest),
             "excused": excused,
             "findings": len(findings)}
    return failures, report_lines, stats


# --- Self-test ---------------------------------------------------------------

def _manifest_row(entry):
    rule = entry["rule"]
    text = entry.get("text", "")
    sha = entry.get("sha") or hashlib.sha256(f"{rule}\n{text.strip()}".encode()).hexdigest()
    fields = [f"rule={rule}", f"class={entry['class']}"]
    if entry.get("caller"):
        fields.append(f"caller={entry['caller']}")
    if entry.get("wave2"):
        fields.append(f"wave2={entry['wave2']}")
    return f"{entry['path']}\t{sha}\t# host-route " + " ".join(fields)


def replay_case(case, root):
    """Replay one host-route case in a private temporary tree and return the
    (failures, report_lines, stats) of the scan."""
    import shutil
    with tempfile.TemporaryDirectory(prefix="host-route-case.") as tmp:
        tmp_root = Path(tmp)
        (tmp_root / "tests" / "fixtures").mkdir(parents=True)
        for name in (POLICY_RELPATH, MAP_SCHEMA_RELPATH, MAP_RELPATH):
            src = root / name
            if src.exists():
                shutil.copyfile(src, tmp_root / name)
        if "map" in case:
            (tmp_root / MAP_RELPATH).write_text(json.dumps(case["map"], indent=2) + "\n",
                                                encoding="utf-8")
        for rel_path, content in case.get("files", {}).items():
            dest = tmp_root / rel_path
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(content, encoding="utf-8")
        rows = ["# uinput inventory (case replay)", "# rows: path sha256 host-route meta"]
        for entry in case.get("inventory", []):
            rows.append(_manifest_row(entry))
        (tmp_root / MANIFEST_RELPATH).write_text("\n".join(rows) + "\n", encoding="utf-8")
        if "map_source" in case:
            meta = dict(case["map_source"])
            map_bytes = (tmp_root / MAP_RELPATH).read_bytes()
            if meta.get("fixture_sha256") == "auto":
                meta["fixture_sha256"] = hashlib.sha256(map_bytes).hexdigest()
            lines = [f"{k}={v}" for k, v in sorted(meta.items())]
            (tmp_root / MAP_SOURCE_RELPATH).write_text("\n".join(lines) + "\n",
                                                       encoding="utf-8")
        return scan_host_routes(tmp_root)


def run_self_test_cases(root):
    """Replay every seeded red / positive control / mutant case from
    host-route-cases-v1.jsonl in one invocation. Returns True when every
    expectation holds."""
    cases_path = root / CASES_RELPATH
    if not cases_path.exists():
        print(f"missing {CASES_RELPATH}", file=sys.stderr)
        return False
    cases = []
    for line in cases_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            cases.append(json.loads(line))
    ok = True
    counts = {"red": 0, "green": 0, "mutant": 0}
    for case in cases:
        expect = case["expect"]
        failures, _report, _stats = replay_case(case, root)
        matched = []
        if expect == "red":
            counts["red"] += 1
            if case.get("mutant"):
                counts["mutant"] += 1
            if case.get("rule"):
                matched = [f for f in failures if f.rule == case["rule"]]
            elif case.get("reason"):
                matched = [f for f in failures if f.detail == case["reason"]]
            verdict = "ok" if matched else "BROKEN"
        else:
            counts["green"] += 1
            matched = []
            verdict = "ok" if not failures else "BROKEN"
        if verdict != "ok":
            ok = False
        shown = ",".join(sorted({f"{f.rule}/{f.detail}" for f in (matched or failures)})) or "-"
        print(f"host_route_case id={case['id']} expect={expect} verdict={verdict} "
              f"failures={len(failures)} matched=[{shown}]")
    print(f"host_route_cases: red={counts['red']} green={counts['green']} "
          f"mutants={counts['mutant']} all_ok={ok}")
    return ok


def seed_creator_self_test():
    """Original creator-inventory seeds: every one must stay red."""
    clean_tree = check(quiet=True)
    seeded_violation_red = not check(extra=[("seeded/violation.c",
                                             'fd = open("/dev/uinput", O_WRONLY);')],
                                     quiet=True)
    live = occurrences()
    if live:
        first = live[0]
        duplicate_live_red = not check(extra=[(first[1], first[3])], quiet=True)
    else:
        # A tree with zero live occurrences (a freshly vendored port, say)
        # still needs the duplicate-live control: synthesize the classified
        # seeded row it duplicates.
        seeded = ("seeded/duplicate.c", 'fd = open("/dev/uinput", O_WRONLY);')
        seeded_allowed = load_manifest()
        seeded_allowed[fingerprint(*seeded)] += 1
        duplicate_live_red = not check(extra=[seeded, seeded], quiet=True,
                                       allowed=seeded_allowed)
    stale_key = "seeded/stale.c\t" + "0" * 64
    allowed = load_manifest()
    allowed[stale_key] += 1
    stale_manifest_red = not check(quiet=True, allowed=allowed)
    return clean_tree, seeded_violation_red, duplicate_live_red, stale_manifest_red


def emit_host_routes(root):
    """Print unclassified host-route occurrence rows for manifest bootstrap."""
    failures, _report, _stats = scan_host_routes(root, use_manifest=True)
    rows = {}
    for finding in failures:
        if finding.detail != "unclassified":
            continue
        rows.setdefault(finding.key, finding)
    for key in sorted(rows):
        finding = rows[key]
        print(f"{key}\t# host-route rule={finding.rule} class=TODO "
              f"detail={finding.detail} at {finding.path}:{finding.line}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--emit", action="store_true",
                        help="print creator-inventory rows for the manifest")
    parser.add_argument("--emit-host-routes", action="store_true",
                        help="print host-route occurrence rows for manifest bootstrap")
    parser.add_argument("--self-test", action="store_true",
                        help="run the seeded red/green controls instead of the tree check")
    args = parser.parse_args(argv)
    if args.emit:
        for key in sorted({item[0] for item in occurrences()}):
            print(key)
        return 0
    if args.emit_host_routes:
        emit_host_routes(ROOT)
        return 0
    if args.self_test:
        clean_tree, seeded_violation_red, duplicate_live_red, stale_manifest_red = \
            seed_creator_self_test()
        creator_ok = (seeded_violation_red and duplicate_live_red and stale_manifest_red
                      and clean_tree)
        if not creator_ok:
            print(f"uinput-inventory: FAIL seeded_violation={seeded_violation_red} "
                  f"duplicate_live={duplicate_live_red} stale_manifest={stale_manifest_red} "
                  f"clean_tree={clean_tree}", file=sys.stderr)
            return 2
        print("uinput-inventory: PASS seeded_violation=red duplicate_live=red "
              "stale_manifest=red clean_tree=green")
        cases_ok = run_self_test_cases(ROOT)
        if not cases_ok:
            return 2
        failures, report_lines, stats = scan_host_routes(ROOT)
        for line in report_lines:
            print(line)
        for finding in failures:
            print(finding.render(), file=sys.stderr)
        if failures:
            print(f"host_route_inventory=FAIL_COUNT count={len(failures)}", file=sys.stderr)
            return 2
        print(f"host_route_inventory=PASS rules={len(CANONICAL_RULES)} "
              f"files={stats['files']} findings_classified={stats['findings_classified']} "
              f"excused={stats['excused']}")
        print("uinput-inventory self-test: host_route=green runs_on=green")
        return 0
    failures, report_lines, stats = scan_host_routes(ROOT)
    ok = check(quiet=False)
    for line in report_lines:
        print(line)
    for finding in failures:
        print(finding.render(), file=sys.stderr)
    if failures:
        print(f"host_route_inventory=FAIL_COUNT count={len(failures)}", file=sys.stderr)
    else:
        print(f"host_route_inventory=PASS rules={len(CANONICAL_RULES)} "
              f"files={stats['files']} findings_classified={stats['findings_classified']} "
              f"excused={stats['excused']}")
    return 0 if (ok and not failures) else 1


if __name__ == "__main__":
    sys.exit(main())
