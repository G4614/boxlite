# 06 AI Agents

Using BoxLite as a sandbox for AI agent workflows.

| File | Description |
|------|-------------|
| `drive_box_with_llm.py` | Let an LLM drive a SimpleBox via tool-use loop (OpenAI) |
| `drive_box_with_minimax.py` | Let MiniMax M3 drive a SimpleBox via tool-use loop |
| `research_agent.py` | Search the web, ask a Codex-compatible reasoner, and answer a question |
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

`research_agent.py` is a minimal no-secrets research loop:

1. Accept a user question.
2. Search the web with DuckDuckGo HTML search, or read fixture results for smoke tests.
3. Send the search context to a Codex-compatible reasoner.
4. Print the final answer.

The default `echo` provider is deterministic and needs no credentials:

```bash
python examples/python/06_ai_agents/research_agent.py \
  --search-provider fixture \
  --search-fixture examples/python/06_ai_agents/research_agent_fixture.json \
  "What can this agent do?"
```

To delegate synthesis to a local Codex-compatible command, set:

```bash
RESEARCH_AGENT_CODEX_PROVIDER=command \
RESEARCH_AGENT_CODEX_COMMAND="codex exec -" \
python examples/python/06_ai_agents/research_agent.py "What is BoxLite?"
```

For a hosted control plane, use `--codex-provider relay`. In relay mode the
agent emits a `codex_request` JSON line and waits for a `codex_response` JSON
line on stdin, so the box only talks to a broker instead of holding a model API
key.
