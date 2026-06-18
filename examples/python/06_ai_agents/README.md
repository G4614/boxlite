# 06 AI Agents

Using BoxLite as a sandbox for AI agent workflows.

| File | Description |
|------|-------------|
| `drive_box_with_llm.py` | Let an LLM drive a SimpleBox via tool-use loop (OpenAI) |
| `drive_box_with_minimax.py` | Let MiniMax M3 drive a SimpleBox via tool-use loop |
| `research_agent.py` | Search the web, ask an LLM through host-side secret substitution, and answer a question |
| `run_codex_in_box.py` | Install and run OpenAI Codex CLI inside a BoxLite box |
| `use_skillbox.py` | Run Claude Code CLI with skills inside a box |
| `chat_with_claude.py` | Multi-turn Claude conversation via stdin JSON protocol |
| `order_starbucks.py` | End-to-end agent: order Starbucks via browser automation |
| `run_openclaw.py` | Run OpenClaw (ClawdBot) AI gateway in a container |

Most examples require `CLAUDE_CODE_OAUTH_TOKEN` to be set.

**Recommended first example:** `drive_box_with_llm.py`

## AI Agent Integration

BoxLite works with any LLM provider to create secure sandboxed environments for AI agents.
The examples in this directory include ready-to-run integrations for
OpenAI and [MiniMax](https://platform.minimax.io) (`MiniMax-M3`, `MiniMax-M2.7`, `MiniMax-M2.7-highspeed`).

## Research Agent

`research_agent.py` is a minimal research loop that can use BoxLite secret
substitution for LLM credentials:

1. Accept a user question.
2. Search the web with DuckDuckGo HTML search, or read fixture results for smoke tests.
3. Send the search context to an OpenAI-compatible chat completion endpoint.
4. Print the final answer.

The default `echo` provider is deterministic and needs no credentials:

```bash
python examples/python/06_ai_agents/research_agent.py \
  --search-provider fixture \
  --search-fixture examples/python/06_ai_agents/research_agent_fixture.json \
  "What can this agent do?"
```

Inside a BoxLite box, pass the real API key as a host-side secret and let the
agent use only the placeholder env var:

```python
box = runtime.create(
    boxlite.BoxOptions(
        image="python:3.12-slim",
        network=boxlite.NetworkSpec(
            mode="enabled",
            allow_net=["api.openai.com"],
        ),
        secrets=[
            boxlite.Secret(
                name="openai_api_key",
                value=os.environ["OPENAI_API_KEY"],
                hosts=["api.openai.com"],
            ),
        ],
    )
)
```

The container sees `BOXLITE_SECRET_OPENAI_API_KEY=<BOXLITE_SECRET:openai_api_key>`.
When the agent calls `https://api.openai.com/v1/chat/completions`, gvproxy
replaces that placeholder with the real key at the network boundary:

```bash
python /root/research_agent.py \
  --answer-provider openai \
  "What is BoxLite?"
```

## Codex CLI In A Box

`run_codex_in_box.py` installs the real `@openai/codex` CLI in a Node.js box,
logs in with a BoxLite secret-backed API key, and runs `codex exec`:

```bash
python examples/python/06_ai_agents/run_codex_in_box.py \
  "Reply exactly: codex inside box works"
```

The script reads `OPENAI_API_KEY` from the current environment first, then
falls back to `~/.config/boxlite/e2e-openai.env` (`OPENAI_API_KEY` or
`BOXLITE_E2E_OPENAI_API_KEY`). Use `--env-file` to point at another file.

The box receives `BOXLITE_SECRET_OPENAI_API_KEY=<BOXLITE_SECRET:openai_api_key>`;
`codex login --with-api-key` stores that placeholder in the box, and gvproxy
substitutes the real key only on outbound requests to `api.openai.com`.

## 测试

这些 agent 示例目前有两条尽量小的测试路径。

### Research Agent

单元测试完全在本地跑，结果是确定性的：

```bash
python -m unittest examples/python/06_ai_agents/test_research_agent.py
```

它会检查 DuckDuckGo HTML 解析、基于 fixture 的 prompt 构造，以及
OpenAI 请求是否使用 BoxLite secret placeholder，而不是明文 API key。

REST e2e 会把 `research_agent.py` 复制进一个真实的 REST-backed box，并在
box 里执行：

```bash
python -m pytest \
  scripts/test/e2e/cases/test_research_agent_example.py::test_research_agent_example_runs_inside_rest_box \
  -vv -s
```

真实 LLM 版本不允许 skip。先在宿主机提供 key，再跑 OpenAI-backed case：

```bash
export BOXLITE_E2E_OPENAI_API_KEY="sk-..."
python -m pytest \
  scripts/test/e2e/cases/test_research_agent_example.py::test_research_agent_openai_provider_uses_boxlite_secret_in_rest_box \
  -vv -s
```

这个 case 会先断言 box 里只能看到
`<BOXLITE_SECRET:openai_api_key>`，然后通过 `api.openai.com` 问模型，最后
确认 agent 输出里没有明文 key，也没有 placeholder。

### Codex In Box

`run_codex_in_box.py` 是一个手动 smoke test，用来验证真实 LLM-backed agent
可以在 box 里跑起来：

```bash
export OPENAI_API_KEY="sk-..."
python examples/python/06_ai_agents/run_codex_in_box.py \
  "Reply exactly: codex inside box works"
```

如果不想每次都 export key，可以把 `OPENAI_API_KEY` 或
`BOXLITE_E2E_OPENAI_API_KEY` 写进 `~/.config/boxlite/e2e-openai.env`，也可以
通过 `--env-file` 指定其他文件。

这条路径会创建一个 Node.js box，安装 `@openai/codex`，用
`BOXLITE_SECRET_OPENAI_API_KEY` 登录，然后执行 `codex exec`。API key 留在宿主机；
box 里只保存和发送 placeholder，真正发往 `api.openai.com` 时由 gvproxy 替换成真实
key。
