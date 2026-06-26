/**
 * Vitest setup file for integration tests.
 *
 * Initializes the default BoxliteRuntime with a unique temp directory
 * to avoid lock contention with any running BoxLite process (e.g.,
 * boxlite-mcp). Follows the same pattern as test-utils/PerTestBoxHome.
 */

import { mkdtempSync, rmSync } from "node:fs";
import { afterAll } from "vitest";
import { getJsBoxlite } from "../lib/native.js";

// Use /tmp/ (not os.tmpdir()) to keep Unix socket paths under macOS
// 104-char SUN_LEN limit. Same pattern as test-utils/PerTestBoxHome.
//
// `make test:integration:node` sets BOXLITE_TEST_HOME_BASE to a per-run temp
// dir it also sweeps via `scripts/test/reap_boxes.sh` on `trap ... EXIT` — the
// only cleanup that survives a vitest hang / VM timeout (which kills the
// process before `afterAll` can run). Honor it so leaked shims land under a
// root the recipe reaps; fall back to a fresh dir for standalone `npm` runs.
const testHome =
  process.env.BOXLITE_TEST_HOME_BASE ?? mkdtempSync("/tmp/boxlite-test-node-");
const testRegistries = [
  { host: "docker.m.daocloud.io", search: true },
  { host: "docker.xuanyuan.me", search: true },
  { host: "docker.1ms.run", search: true },
  { host: "docker.io", search: true },
];

const Boxlite = getJsBoxlite();
Boxlite.initDefault({ homeDir: testHome, imageRegistries: testRegistries });

afterAll(async () => {
  try {
    // Get the default runtime and shut it down
    const runtime = Boxlite.withDefaultConfig();
    await runtime.shutdown();
  } catch {
    // Ignore shutdown errors during cleanup
  }
  rmSync(testHome, { recursive: true, force: true });
});
