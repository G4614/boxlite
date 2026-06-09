"""E2E port of `sdks/python/tests/test_box_management.py`.

Covers create/list/get/remove via the SDK, but in REST mode against
the local API. The source file uses Boxlite.default() (local FFI) —
this version uses Boxlite.rest() so a regression in the proxy
controller surfaces.
"""
from __future__ import annotations

import asyncio

import pytest

from conftest import drain


@pytest.mark.asyncio
async def test_create_named_box(rt, image):
    """Box created with an explicit name carries it through to
    get_info. Uses raw rt.create because `box_factory` doesn't take a
    `name=` kwarg through BoxOptions — name is a runtime-level arg."""
    import boxlite
    name = "e2e-test-box"
    b = await rt.create(
        boxlite.BoxOptions(image=image, auto_remove=True), name=name,
    )
    try:
        info = await rt.get_info(b.id)
        assert info is not None
        assert getattr(info, "name", "") == name, (
            f"name not propagated: got {getattr(info,'name',None)!r}"
        )
    finally:
        try:
            await rt.remove(b.id, force=True)
        except Exception:
            pass


@pytest.mark.asyncio
async def test_list_info_includes_created_box(rt, box):
    infos = await rt.list_info()
    ids = {info.id for info in infos}
    assert box.id in ids, f"created box not in list: {ids}"


@pytest.mark.asyncio
async def test_box_options_env_propagates_through_rest(box_factory):
    """env on BoxOptions must reach the guest. Uses box_factory because
    BoxOptions(env=...) is per-box and the default `box` fixture
    doesn't expose option overrides."""
    b = await box_factory(env=[("BOXLITE_E2E_MARKER", "yes-its-there")])
    ex = await b.exec("sh", ["-c", "echo $BOXLITE_E2E_MARKER"], None)
    out, _ = await drain(ex)
    await asyncio.wait_for(ex.wait(), timeout=30)
    assert "yes-its-there" in out, (
        f"env from BoxOptions did not reach the guest: {out!r}"
    )
