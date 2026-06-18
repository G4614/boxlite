"""Cloud/REST smoke coverage for the research-agent example.

This proves the example can be copied into and executed inside a REST-backed
box. The default test uses the deterministic echo provider. An optional OpenAI
test uses BoxLite secret substitution so the real API key stays host-side.
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


async def _create_openai_research_box(rt, image, api_key):
    box_image = _research_image(image)
    box = await rt.create(
        boxlite.BoxOptions(
            image=box_image,
            auto_remove=True,
            network=boxlite.NetworkSpec(
                mode="enabled",
                allow_net=["api.openai.com"],
            ),
            secrets=[
                boxlite.Secret(
                    name="openai_api_key",
                    value=api_key,
                    hosts=["api.openai.com"],
                )
            ],
        )
    )
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
async def test_research_agent_openai_provider_uses_boxlite_secret_in_rest_box(rt, image):
    """Run the agent against a real LLM API without putting the key in the VM."""
    api_key = os.environ.get("BOXLITE_E2E_OPENAI_API_KEY")
    if not api_key:
        pytest.fail("BOXLITE_E2E_OPENAI_API_KEY is required for the real LLM e2e")

    box, box_image, python_bin = await _create_openai_research_box(rt, image, api_key)
    try:
        env_ex = await box.exec(
            "sh",
            ["-lc", "printenv BOXLITE_SECRET_OPENAI_API_KEY || true"],
            None,
        )
        env_out, env_err = await drain(env_ex)
        env_result = await asyncio.wait_for(env_ex.wait(), timeout=30)
        assert env_result.exit_code == 0, (
            f"failed to inspect secret placeholder env in REST box image={box_image}: "
            f"stdout={env_out!r} stderr={env_err!r}"
        )
        assert env_out.strip() == "<BOXLITE_SECRET:openai_api_key>", (
            "REST box did not receive the OpenAI secret placeholder env; "
            f"stdout={env_out!r} stderr={env_err!r}"
        )

        ex = await box.exec(
            python_bin,
            [
                "/root/research_agent.py",
                "--search-provider",
                "fixture",
                "--search-fixture",
                "/root/research_agent_fixture.json",
                "--answer-provider",
                "openai",
                "--openai-model",
                os.environ.get("BOXLITE_E2E_OPENAI_MODEL", "gpt-4.1-mini"),
                "What can this agent do?",
            ],
            None,
        )
        out, err = await drain(ex)
        result = await asyncio.wait_for(ex.wait(), timeout=120)

        assert result.exit_code == 0, (
            f"research_agent.py OpenAI mode failed in REST box image={box_image}: "
            f"stdout={out!r} stderr={err!r}"
        )
        assert "BoxLite" in out or "sandbox" in out.lower()
        assert "sk-" not in out
        assert "<BOXLITE_SECRET:openai_api_key>" not in out
    finally:
        try:
            await rt.remove(box.id, force=True)
        except Exception:
            pass
