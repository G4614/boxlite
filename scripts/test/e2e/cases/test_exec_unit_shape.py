"""E2E port of `sdks/python/tests/test_exec.py`.

The source file is a unit-test of the `ExecResult` dataclass — it
doesn't need a VM. We keep a real e2e shape check here so that a
breaking REST → dataclass mapping change shows up: an actual exec
must return an object with `exit_code`, `stdout`, `stderr` fields
populated.
"""
from __future__ import annotations

import asyncio

import pytest

from conftest import drain


@pytest.mark.asyncio
async def test_exec_result_shape_via_rest(box):
    """A round-trip exec returns a result-like object whose exit_code,
    stdout, stderr are correctly typed and reflect the command."""
    ex = await box.exec(
        "sh", ["-c", "echo to-stdout; echo to-stderr >&2; exit 3"], None
    )
    out, err = await drain(ex)
    rc = await asyncio.wait_for(ex.wait(), timeout=30)

    assert isinstance(rc.exit_code, int), (
        f"exit_code is not int: {type(rc.exit_code)}"
    )
    assert rc.exit_code == 3, f"exit_code wrong: {rc.exit_code}"
    assert "to-stdout" in out, f"stdout: {out!r}"
    assert "to-stderr" in err, f"stderr: {err!r}"
