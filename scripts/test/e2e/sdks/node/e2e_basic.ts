// Minimal Node SDK e2e smoke driver, called by cases/test_node_entry.py.
//
// Imports the LOCAL sdks/node TypeScript source directly (we run under
// `npx tsx`, which compiles TS on the fly). The package `main` points at
// `dist/index.js`, which the CI install step does not produce — it only
// stages the napi binary (native/ + npm/). Importing `lib/index.ts`
// exercises current code without a separate `tsc` build, and the binding
// still loads via lib/native.ts → ../native/boxlite.js.
//
// Like the C SDK smoke, this only does create + remove — that exercises
// the napi-rs binding's URL/credential/options marshalling end to end.
// Exec stdout streaming is covered by the Python / Go / CLI smokes.

import {
  JsBoxlite, BoxliteRestOptions, ApiKeyCredential,
} from '../../../../../sdks/node/lib/index.ts';

function env(k: string, def: string): string {
  const v = process.env[k];
  return v && v.length ? v : def;
}

function die(msg: string): never {
  console.error(`FATAL: ${msg}`);
  process.exit(2);
}

(async () => {
  const url = env('BOXLITE_E2E_URL', 'http://localhost:3000/api');
  const apiKey = env('BOXLITE_E2E_API_KEY', 'devkey');
  const prefix = env('BOXLITE_E2E_PREFIX', '');
  const image = env('BOXLITE_E2E_IMAGE', 'alpine:3.23');

  const rt = JsBoxlite.rest(new BoxliteRestOptions({
    url,
    credential: new ApiKeyCredential(apiKey),
    pathPrefix: prefix,
  }));

  let boxId: string | null = null;
  try {
    const box = await rt.create({ image, autoRemove: true });
    boxId = box.id;
    console.log(`BOX_ID=${boxId}`);
  } catch (e: any) {
    die(`error: ${e.message ?? e}`);
  } finally {
    if (boxId) {
      try { await rt.remove(boxId, true); } catch { /* best-effort */ }
    }
  }

  console.log('OK');
})();
