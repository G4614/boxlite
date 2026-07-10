import { afterEach, describe, expect, test, vi } from "vitest";

vi.mock("../lib/native.js", () => ({
  getJsBoxlite: () => ({
    withDefaultConfig: () => ({
      create: async () => ({ id: "unused" }),
      getOrCreate: async () => ({ box: { id: "unused" }, created: false }),
    }),
  }),
}));

describe("SimpleBox port preview URLs", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    process.env.BOXLITE_REST_URL = undefined;
    process.env.BOXLITE_API_KEY = undefined;
    process.env.BOXLITE_REST_PATH_PREFIX = undefined;
    globalThis.fetch = originalFetch;
    vi.restoreAllMocks();
  });

  test("gets a direct preview URL through the REST API", async () => {
    const { SimpleBox } = await import("../lib/simplebox.js");
    process.env.BOXLITE_REST_URL = "https://api.example.com/api/";
    process.env.BOXLITE_API_KEY = "blk_test";
    process.env.BOXLITE_REST_PATH_PREFIX = "org-1";

    const fetchMock = vi.fn(async () => ({
      ok: true,
      json: async () => ({ url: "https://3000-box.proxy.example.com" }),
    }));
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const box = new SimpleBox({ image: "alpine:latest" }) as SimpleBox & {
      _box: { id: string };
    };
    box._box = { id: "BoxABC123xyz" };

    await expect(box.getPortPreviewUrl(3000)).resolves.toBe(
      "https://3000-box.proxy.example.com",
    );
    expect(fetchMock).toHaveBeenCalledWith(
      "https://api.example.com/api/v1/org-1/box/BoxABC123xyz/ports/3000/preview-url",
      {
        headers: {
          Accept: "application/json",
          Authorization: "Bearer blk_test",
        },
      },
    );
  });

  test("rejects invalid preview ports", async () => {
    const { SimpleBox } = await import("../lib/simplebox.js");
    process.env.BOXLITE_REST_URL = "https://api.example.com/api";
    process.env.BOXLITE_API_KEY = "blk_test";

    const box = new SimpleBox({ image: "alpine:latest" }) as SimpleBox & {
      _box: { id: string };
    };
    box._box = { id: "BoxABC123xyz" };

    await expect(box.getPortPreviewUrl(0)).rejects.toThrow(
      "port must be an integer between 1 and 65535",
    );
  });
});
