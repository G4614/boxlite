import importlib.util
import sys
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("research_agent.py")
spec = importlib.util.spec_from_file_location("research_agent", MODULE_PATH)
research_agent = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules["research_agent"] = research_agent
spec.loader.exec_module(research_agent)


class ResearchAgentTest(unittest.TestCase):
    def test_duckduckgo_lite_parser_extracts_results(self):
        parser = research_agent.DuckDuckGoHTMLParser()
        parser.feed(
            """
            <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com" class='result-link'>
              Example Result
            </a>
            <td class='result-snippet'>A useful result snippet.</td>
            """
        )

        self.assertEqual(len(parser.results), 1)
        self.assertEqual(parser.results[0].title, "Example Result")
        self.assertEqual(parser.results[0].url, "https://example.com")
        self.assertEqual(parser.results[0].snippet, "A useful result snippet.")

    def test_fixture_search_and_prompt_include_citations(self):
        fixture = Path(__file__).with_name("research_agent_fixture.json")
        results = research_agent.search_from_fixture(str(fixture), limit=2)
        prompt = research_agent.build_codex_prompt("Question?", results)

        self.assertIn("Question?", prompt)
        self.assertIn("[1] BoxLite AI agent examples", prompt)
        self.assertIn("[2] Codex tool-use loop", prompt)


if __name__ == "__main__":
    unittest.main()
