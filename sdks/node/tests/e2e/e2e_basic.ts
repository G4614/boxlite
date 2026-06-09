// Minimal Node SDK e2e smoke driver, invoked by the sibling test_node_entry.py
// via tsx.
//
// Imports from the LOCAL sdks/node build (the repo root has a stale
// @boxlite-ai/boxlite 0.9.5 install with field-name glitches; we want
// e2e to test current code, not last release).
//
// Exercises create + exec + stdout drain + remove — the exec+drain step
// is what catches stream-marshaling regressions on the napi side that
// create+remove alone would miss (see #563 stdout-drop in the Python
// SDK for a cousin bug).

import {
  JsBoxlite,
  BoxliteRestOptions,
  ApiKeyCredential,
} from "../../lib/index.ts";

function env(k: string, def: string): string {
  const v = process.env[k];
  return v && v.length ? v : def;
}

function die(msg: string): never {
  console.error(`FATAL: ${msg}`);
  process.exit(2);
}

(async () => {
  const url = env("BOXLITE_E2E_URL", "http://localhost:3000/api");
  const apiKey = env("BOXLITE_E2E_API_KEY", "devkey");
  const prefix = env("BOXLITE_E2E_PREFIX", "");
  const image = env("BOXLITE_E2E_IMAGE", "alpine:3.23");

  const rt = JsBoxlite.rest(
    new BoxliteRestOptions({
      url,
      credential: new ApiKeyCredential(apiKey),
      pathPrefix: prefix,
    }),
  );

  let boxId: string | null = null;
  let stdoutAccum = "";
  let execExitCode = -1;
  try {
    const box = await rt.create({ image, autoRemove: true });
    boxId = box.id;
    console.log(`BOX_ID=${boxId}`);

    // Exec + drain stdout: this is the leg that catches
    // napi-side stream-marshaling regressions. Plain create + remove
    // would silently miss e.g. a dropped chunk or a wrong-encoding map.
    const execution = await box.exec("echo", ["HELLO-FROM-NODE"]);
    const stdout = await execution.stdout();
    while (true) {
      const chunk = await stdout.next();
      if (chunk === null) break;
      stdoutAccum += chunk;
    }
    const result = await execution.wait();
    execExitCode = result.exitCode;
    console.log(`EXIT_CODE=${execExitCode}`);
    console.log(`STDOUT=${stdoutAccum.replace(/\n$/, "")}`);
  } catch (e: any) {
    die(`error: ${e.message ?? e}`);
  } finally {
    if (boxId) {
      try {
        await rt.remove(boxId, true);
      } catch {
        /* best-effort */
      }
    }
  }

  if (execExitCode !== 0) {
    die(`exec exit_code=${execExitCode}`);
  }
  if (!stdoutAccum.includes("HELLO-FROM-NODE")) {
    die(`exec stdout missing marker: ${JSON.stringify(stdoutAccum)}`);
  }
  console.log("OK");
})();
