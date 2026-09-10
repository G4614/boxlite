"""What survives a stop→start: a declared service vs. an exec-started one.

A stopped box keeps its disk but not its processes — `start` reboots the VM,
rebuilds the container and re-runs its init. So the only thing that brings a
user's service back on its own is the one command the platform replays:
`entrypoint`/`cmd`. A service launched with `exec` (the natural thing for an
agent to do, `nohup … &`) is gone, and no amount of auto-resume brings it back.

That asymmetry is a contract users have to design around — auto_resume returns
a running *box*, not a running *service* — so it is pinned here in both
directions. The negative case is as load-bearing as the positive one: if
exec-started processes ever do survive, the guidance built on this changes.
"""
from __future__ import annotations

import asyncio

import boxlite
import pytest

from conftest import drain

PORT = 3000
SERVICE_ARGV = ["-m", "http.server", str(PORT), "--bind", "0.0.0.0"]


async def _run(box, script: str, timeout: int = 30) -> int:
    ex = await box.exec("sh", ["-c", script])
    await drain(ex)
    rc = await asyncio.wait_for(ex.wait(), timeout=timeout)
    return rc.exit_code


async def _is_serving(box) -> bool:
    """True when something answers on PORT from inside the box.

    A failing exec counts as "not serving": right after a start the box can
    briefly refuse execs, and that is the same answer as an empty port for
    our purposes — the caller polls.
    """
    try:
        code = await _run(box, f"curl -sf --max-time 5 http://127.0.0.1:{PORT}/ >/dev/null")
    except Exception:
        return False
    return code == 0


async def _restart(box) -> None:
    """stop→start, tolerating the window where the state change is still in
    flight — `start` right after `stop` can be refused, and the retry is the
    point of the test, not something to paper over with a fixed sleep."""
    await box.stop()
    deadline = asyncio.get_running_loop().time() + 60
    last: Exception | None = None
    while asyncio.get_running_loop().time() < deadline:
        try:
            await box.start()
            return
        except Exception as exc:  # state change in progress
            last = exc
            await asyncio.sleep(2)
    raise AssertionError(f"could not start the box again: {last}")


async def _wait_exec_ready(box, timeout: float = 60.0) -> bool:
    """Poll a trivial exec until the box accepts one.

    Needed before the negative assertion: `_is_serving` reports a failing exec
    as "not serving", so a box that never becomes usable at all would make the
    absence check pass without measuring anything.
    """
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        try:
            if await _run(box, "true") == 0:
                return True
        except Exception:
            pass
        await asyncio.sleep(2)
    return False


async def _wait_serving(box, timeout: float = 45.0) -> bool:
    """Poll rather than sleep a fixed amount: a cold boot re-runs init, and how
    long the service needs is the box's business, not a constant we can pick."""
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        if await _is_serving(box):
            return True
        await asyncio.sleep(2)
    return False


@pytest.mark.asyncio
async def test_entrypoint_declared_service_survives_stop_start(rt, image):
    """A service declared as entrypoint/cmd is serving again after a resume,
    with nothing restarted by hand."""
    box = await rt.create(
        boxlite.BoxOptions(
            image=image,
            auto_remove=False,
            entrypoint=["python3"],
            cmd=SERVICE_ARGV,
        )
    )
    try:
        assert await _wait_serving(box), "declared service never came up before the stop"

        await _restart(box)

        assert await _wait_serving(box), (
            "a service declared via entrypoint/cmd did not come back after "
            "stop→start; container init is expected to replay it"
        )
    finally:
        await rt.remove(box.id, force=True)


@pytest.mark.asyncio
async def test_exec_started_service_does_not_survive_stop_start(rt, image):
    """The contrast: the same service started with exec is gone after a resume.

    Guards the guidance, not a desired behaviour — stopping a box kills every
    process in it, so a service the platform was never told about cannot be
    replayed.
    """
    box = await rt.create(boxlite.BoxOptions(image=image, auto_remove=False))
    try:
        argv = " ".join(["python3", *SERVICE_ARGV])
        await _run(box, f"nohup {argv} >/tmp/service.log 2>&1 &")
        assert await _wait_serving(box), "exec-started service never came up before the stop"

        await _restart(box)

        # Establish that the box can run commands again *before* reading
        # anything into a failed probe — otherwise "no service" and "no box"
        # are the same observation and the assertion below proves nothing.
        assert await _wait_exec_ready(box), (
            "box never accepted an exec after the restart, so its service "
            "state could not be measured"
        )

        # Give it at least as long as the positive case gets, so a pass here
        # means "still absent", not "we did not wait long enough".
        assert not await _wait_serving(box, timeout=20.0), (
            "an exec-started process survived stop→start — box restart is "
            "expected to kill it; if this is now intended, the auto-resume "
            "guidance and the service-revival design need revisiting"
        )
    finally:
        await rt.remove(box.id, force=True)
