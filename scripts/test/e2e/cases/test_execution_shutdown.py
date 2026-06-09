"""E2E port of `src/boxlite/tests/execution_shutdown.rs`.

Verifies the behaviour of exec and box state during/after box.stop():
pending exec.wait() should resolve, new exec attempts on a stopped
box should be cleanly rejected (not 5xx).

These tests cannot use the default `box` fixture because they
explicitly stop the box mid-flow. With auto_remove=True the runner
would tear down the VM as soon as stop() fires, masking the bug
the test is here to catch (wait() never resolving, or stopped-box
exec returning 5xx instead of a typed error). The `box_factory`
fixture still tracks the created ids so the autouse path-verification
fixture observes them and force-removes on teardown.
"""
from __future__ import annotations

import asyncio

import pytest

from conftest import drain


@pytest.mark.asyncio
async def test_wait_resolves_after_box_stop(box_factory):
    box = await box_factory(auto_remove=False)
    ex = await box.exec("sh", ["-c", "sleep 60"], None)
    # Stop the box while exec is still running. wait() should resolve
    # (with whatever exit code the runtime reports) within a few
    # seconds, not hang.
    await asyncio.sleep(0.5)
    await box.stop()
    try:
        rc = await asyncio.wait_for(ex.wait(), timeout=30)
        # Whatever exit code is fine; the point is it resolved.
        assert rc is not None
    except asyncio.TimeoutError:
        pytest.fail("ex.wait() did not resolve within 30s after box.stop()")


@pytest.mark.asyncio
async def test_exec_on_stopped_box_is_typed_error(box_factory):
    """Trying to exec on a stopped box must return a typed client
    error (not 5xx). Catches API/runner mapping regressions."""
    box = await box_factory(auto_remove=False)
    await box.stop()
    # Now try to exec — should fail with a clean client error
    with pytest.raises(Exception) as exc_info:
        ex = await box.exec("sh", ["-c", "echo nope"], None)
        await drain(ex)
        await ex.wait()
    msg = str(exc_info.value)
    assert "500" not in msg and "Internal" not in msg, (
        f"exec on stopped box returned 5xx: {msg!r}"
    )
