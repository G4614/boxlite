from __future__ import annotations

import json

import pytest

from boxlite.simplebox import _preview_api_url, _request_preview_url


def test_preview_api_url_uses_rest_env(monkeypatch):
    monkeypatch.setenv("BOXLITE_REST_URL", "https://api.example.com/api/")
    monkeypatch.setenv("BOXLITE_REST_PATH_PREFIX", "org-1")

    assert (
        _preview_api_url("BoxABC123xyz", 3000)
        == "https://api.example.com/api/v1/org-1/box/BoxABC123xyz/ports/3000/preview-url"
    )


def test_request_preview_url_returns_response_url(monkeypatch):
    monkeypatch.setenv("BOXLITE_REST_URL", "https://api.example.com/api")
    monkeypatch.setenv("BOXLITE_API_KEY", "blk_test")
    monkeypatch.delenv("BOXLITE_REST_PATH_PREFIX", raising=False)

    class Response:
        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return None

        def read(self):
            return json.dumps({"url": "https://3000-token.proxy.example.com"}).encode()

    captured = {}

    def fake_urlopen(request, timeout):
        captured["url"] = request.full_url
        captured["auth"] = request.headers["Authorization"]
        captured["timeout"] = timeout
        return Response()

    monkeypatch.setattr("boxlite.simplebox.urlopen", fake_urlopen)

    url = _request_preview_url("BoxABC123xyz", 3000)

    assert url == "https://3000-token.proxy.example.com"
    assert captured == {
        "url": "https://api.example.com/api/v1/box/BoxABC123xyz/ports/3000/preview-url",
        "auth": "Bearer blk_test",
        "timeout": 30,
    }


def test_preview_api_url_rejects_invalid_port(monkeypatch):
    monkeypatch.setenv("BOXLITE_REST_URL", "https://api.example.com/api")

    with pytest.raises(ValueError):
        _preview_api_url("box", 0)
