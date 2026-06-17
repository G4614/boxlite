#!/usr/bin/env python3
"""
Research agent example.

Accepts a question, searches the web, asks a Codex-compatible reasoner to
synthesize an answer, and prints the final response.

This example intentionally keeps provider credentials out of the agent. The
reasoning step can be backed by a local command, a control-plane relay, or a
deterministic echo mode for smoke tests.
"""

from __future__ import annotations

import argparse
import html
import json
import os
import shlex
import subprocess
import sys
import textwrap
import urllib.parse
import urllib.request
from dataclasses import dataclass
from html.parser import HTMLParser
from typing import Iterable


DEFAULT_USER_AGENT = "boxlite-research-agent/0.1"


@dataclass
class SearchResult:
    title: str
    url: str
    snippet: str


class DuckDuckGoHTMLParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.results: list[SearchResult] = []
        self._in_title = False
        self._in_snippet = False
        self._title_parts: list[str] = []
        self._snippet_parts: list[str] = []
        self._current_url = ""

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_dict = dict(attrs)
        classes = attrs_dict.get("class", "")
        if tag == "a" and ("result__a" in classes or "result-link" in classes):
            self._in_title = True
            self._title_parts = []
            self._snippet_parts = []
            self._current_url = attrs_dict.get("href") or ""
        elif tag in {"a", "div", "td"} and ("result__snippet" in classes or "result-snippet" in classes):
            self._in_snippet = True

    def handle_endtag(self, tag: str) -> None:
        if tag == "a" and self._in_title:
            self._in_title = False
        elif tag in {"a", "div", "td"} and self._in_snippet:
            self._in_snippet = False
            title = clean_text(" ".join(self._title_parts))
            snippet = clean_text(" ".join(self._snippet_parts))
            if title and self._current_url:
                self.results.append(SearchResult(title=title, url=normalize_ddg_url(self._current_url), snippet=snippet))

    def handle_data(self, data: str) -> None:
        if self._in_title:
            self._title_parts.append(data)
        elif self._in_snippet:
            self._snippet_parts.append(data)


def clean_text(value: str) -> str:
    return " ".join(html.unescape(value).split())


def normalize_ddg_url(url: str) -> str:
    if url.startswith("//"):
        url = f"https:{url}"
    parsed = urllib.parse.urlparse(url)
    query = urllib.parse.parse_qs(parsed.query)
    if "uddg" in query:
        return query["uddg"][0]
    return url


def search_duckduckgo(question: str, limit: int, timeout: float) -> list[SearchResult]:
    params = urllib.parse.urlencode({"q": question})
    for endpoint in ("https://html.duckduckgo.com/html/", "https://lite.duckduckgo.com/lite/"):
        request = urllib.request.Request(
            f"{endpoint}?{params}",
            headers={"User-Agent": DEFAULT_USER_AGENT},
        )
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read().decode("utf-8", errors="replace")

        parser = DuckDuckGoHTMLParser()
        parser.feed(body)
        if parser.results:
            return parser.results[:limit]

    return []


def search_from_fixture(path: str, limit: int) -> list[SearchResult]:
    with open(path, "r", encoding="utf-8") as handle:
        payload = json.load(handle)
    results = []
    for item in payload[:limit]:
        results.append(SearchResult(title=item["title"], url=item["url"], snippet=item.get("snippet", "")))
    return results


def format_context(results: Iterable[SearchResult]) -> str:
    lines = []
    for index, result in enumerate(results, start=1):
        lines.append(f"[{index}] {result.title}\nURL: {result.url}\nSnippet: {result.snippet}")
    return "\n\n".join(lines)


def build_codex_prompt(question: str, results: list[SearchResult]) -> str:
    return textwrap.dedent(
        f"""
        You are a concise research agent. Answer the user's question using the
        web search results below. Cite sources inline as [1], [2], etc. If the
        search results are insufficient, say what is missing.

        Question:
        {question}

        Search results:
        {format_context(results)}
        """
    ).strip()


def ask_codex_command(prompt: str, command: str, timeout: float) -> str:
    proc = subprocess.run(
        shlex.split(command),
        input=prompt,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"Codex command failed with exit code {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout.strip()


def ask_codex_relay(prompt: str) -> str:
    print(json.dumps({"type": "codex_request", "prompt": prompt}), flush=True)
    line = sys.stdin.readline()
    if not line:
        raise RuntimeError("relay mode expected a JSON response on stdin")
    payload = json.loads(line)
    if payload.get("type") != "codex_response":
        raise RuntimeError(f"unexpected relay response: {payload}")
    return str(payload.get("answer", "")).strip()


def ask_echo(question: str, results: list[SearchResult]) -> str:
    if not results:
        return f"No search results were available for: {question}"
    bullets = "\n".join(f"- [{idx}] {result.title}: {result.snippet}" for idx, result in enumerate(results, start=1))
    return f"Echo provider summary for: {question}\n\n{bullets}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Search the web, ask Codex, and answer a question.")
    parser.add_argument("question", nargs="+", help="Question to answer")
    parser.add_argument("--search-provider", choices=["duckduckgo", "fixture"], default=os.getenv("RESEARCH_AGENT_SEARCH_PROVIDER", "duckduckgo"))
    parser.add_argument("--search-fixture", default=os.getenv("RESEARCH_AGENT_SEARCH_FIXTURE"))
    parser.add_argument("--codex-provider", choices=["command", "relay", "echo"], default=os.getenv("RESEARCH_AGENT_CODEX_PROVIDER", "echo"))
    parser.add_argument("--codex-command", default=os.getenv("RESEARCH_AGENT_CODEX_COMMAND"))
    parser.add_argument("--limit", type=int, default=int(os.getenv("RESEARCH_AGENT_SEARCH_LIMIT", "5")))
    parser.add_argument("--timeout", type=float, default=float(os.getenv("RESEARCH_AGENT_TIMEOUT", "20")))
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    question = " ".join(args.question)

    if args.search_provider == "fixture":
        if not args.search_fixture:
            raise SystemExit("--search-fixture is required with fixture search provider")
        results = search_from_fixture(args.search_fixture, args.limit)
    else:
        results = search_duckduckgo(question, args.limit, args.timeout)

    prompt = build_codex_prompt(question, results)
    if args.codex_provider == "command":
        if not args.codex_command:
            raise SystemExit("--codex-command or RESEARCH_AGENT_CODEX_COMMAND is required")
        answer = ask_codex_command(prompt, args.codex_command, args.timeout)
    elif args.codex_provider == "relay":
        answer = ask_codex_relay(prompt)
    else:
        answer = ask_echo(question, results)

    print(answer)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
