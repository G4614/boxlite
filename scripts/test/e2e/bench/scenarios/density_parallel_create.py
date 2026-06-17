"""Parallel box creation: fire N creates concurrently, measure per-box and batch wall."""
from __future__ import annotations
import concurrent.futures
import time
from bench.harness import create_box, delete_box

SCENARIO = "density-parallel-create"
BOX_COUNT = 5


def _create_one(idx: int) -> tuple[str, float]:
    t = time.monotonic()
    bid = create_box()
    return bid, (time.monotonic() - t) * 1000


def run_once(iteration: int) -> dict[str, float]:
    t0 = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=BOX_COUNT) as pool:
        futs = [pool.submit(_create_one, i) for i in range(BOX_COUNT)]
        results = [f.result() for f in concurrent.futures.as_completed(futs)]
    batch_wall = (time.monotonic() - t0) * 1000

    bids = [r[0] for r in results]
    per_box = [r[1] for r in results]

    try:
        return {
            "batch_wall_ms": batch_wall,
            "box_count": float(BOX_COUNT),
            "per_box_mean_ms": sum(per_box) / len(per_box),
            "per_box_max_ms": max(per_box),
            "per_box_min_ms": min(per_box),
        }
    finally:
        for bid in bids:
            delete_box(bid)
