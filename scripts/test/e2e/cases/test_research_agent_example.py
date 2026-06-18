"""Cloud/REST smoke coverage for the research-agent example.

This proves the example can be copied into and executed inside a REST-backed
box. The tests intentionally avoid model API tokens in the box: one uses the
deterministic echo provider, and one exercises the relay protocol with a
synthetic Codex response.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

import boxlite
import pytest

from conftest import CRED_PATH, DEFAULT_PROFILE, drain


REPO = Path(__file__).resolve().parents[4]
RESEARCH_AGENT = REPO / "examples/python/06_ai_agents/research_agent.py"
RESEARCH_FIXTURE = REPO / "examples/python/06_ai_agents/research_agent_fixture.json"
DEFAULT_CLOUD_PYTHON_IMAGE = "ghcr.io/boxlite-ai/boxlite-agent-python:20260605-p0-r3"


def _supported_images() -> list[str]:
    if not CRED_PATH.exists():
        return []
    try:
        profile = tomllib.loads(CRED_PATH.read_text())["profiles"][DEFAULT_PROFILE]
        url = (
            f"{profile['url'].rstrip('/')}/v1/{profile.get('path_prefix') or ''}/boxes"
            .replace("//boxes", "/boxes")
        )
        req = urllib.request.Request(
            url,
            method="POST",
            headers={
                "Authorization": f"Bearer {profile['api_key']}",
                "Content-Type": "application/json",
            },
            data=json.dumps({
                "image": "__research_agent_probe_not_supported__",
                "cpus": 1,
                "memory_mib": 256,
            }).encode(),
        )
        urllib.request.urlopen(req, timeout=10).read()
    except urllib.error.HTTPError as exc:
        if exc.code != 400:
            return []
        body = exc.read().decode("utf-8", "replace")
        match = re.search(r"Supported images:\s*(.+?)\s*(?:\"|$)", body)
        if not match:
            return []
        return [item.strip() for item in match.group(1).split(",") if item.strip()]
    except Exception:
        return []
    return []


def _research_image(default_image: str) -> str:
    explicit = os.environ.get("BOXLITE_E2E_RESEARCH_IMAGE")
    if explicit:
        return explicit
    supported = _supported_images()
    if DEFAULT_CLOUD_PYTHON_IMAGE in supported:
        return DEFAULT_CLOUD_PYTHON_IMAGE
    for image in supported:
        if "python" in image:
            return image
    return default_image


async def _create_research_box(rt, image):
    box_image = _research_image(image)
    box = await rt.create(boxlite.BoxOptions(image=box_image, auto_remove=True))
    ex = await box.exec(
        "sh",
        ["-lc", "command -v python3 || command -v python"],
        None,
    )
    out, err = await drain(ex)
    result = await asyncio.wait_for(ex.wait(), timeout=30)
    if result.exit_code != 0:
        try:
            await rt.remove(box.id, force=True)
        except Exception:
            pass
        pytest.skip(f"box image {box_image!r} has no python interpreter: {err!r}")

    await box.copy_in(str(RESEARCH_AGENT), "/root/research_agent.py")
    await box.copy_in(str(RESEARCH_FIXTURE), "/root/research_agent_fixture.json")
    return box, box_image, out.strip().splitlines()[0]


@pytest.mark.asyncio
async def test_research_agent_example_runs_inside_rest_box(rt, image):
    box, box_image, python_bin = await _create_research_box(rt, image)
    try:
        ex = await box.exec(
            python_bin,
            [
                "/root/research_agent.py",
                "--search-provider",
                "fixture",
                "--search-fixture",
                "/root/research_agent_fixture.json",
                "What can this agent do?",
            ],
            None,
        )
        out, err = await drain(ex)
        result = await asyncio.wait_for(ex.wait(), timeout=60)

        assert result.exit_code == 0, (
            f"research_agent.py failed in REST box image={box_image}: "
            f"stdout={out!r} stderr={err!r}"
        )
        assert "Echo provider summary for: What can this agent do?" in out
        assert "BoxLite AI agent examples" in out
        assert "Codex tool-use loop" in out
    finally:
        try:
            await rt.remove(box.id, force=True)
        except Exception:
            pass


@pytest.mark.asyncio
async def test_research_agent_relay_provider_accepts_codex_response_in_rest_box(rt, image):
    """The relay provider emits a Codex request and consumes a response.

    This pins the no-token-in-box interaction shape without depending on a real
    LLM service in CI. The synthetic response stands in for the control plane's
    Codex reply.
    """
    box, box_image, python_bin = await _create_research_box(rt, image)
    try:
        response = json.dumps({
            "type": "codex_response",
            "answer": "Relay answer: this agent can search, ask Codex, and answer.",
        })
        command = (
            f"printf '%s\\n' {response!r} | "
            f"{python_bin} /root/research_agent.py "
            "--search-provider fixture "
            "--search-fixture /root/research_agent_fixture.json "
            "--codex-provider relay "
            "'What can this agent do?'"
        )
        ex = await box.exec("sh", ["-lc", command], None)
        out, err = await drain(ex)
        result = await asyncio.wait_for(ex.wait(), timeout=60)

        assert result.exit_code == 0, (
            f"research_agent.py relay mode failed in REST box image={box_image}: "
            f"stdout={out!r} stderr={err!r}"
        )
        assert '"type": "codex_request"' in out
        assert "Question:" in out
        assert "What can this agent do?" in out
        assert "Relay answer: this agent can search, ask Codex, and answer." in out
    finally:
        try:
            await rt.remove(box.id, force=True)
        except Exception:
            pass
