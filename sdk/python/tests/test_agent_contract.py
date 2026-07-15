import json
import unittest
from pathlib import Path

from muzen.agent import (
    AgentBudget,
    AgentDefinition,
    AgentInput,
    MuzenError,
    PutSecretInput,
    TextBlock,
    _agent_definition_from_wire,
    define_agent,
    normalize_agent_input,
    to_wire,
)


def fixture():
    path = Path(__file__).resolve().parents[3] / "fixtures" / "agent-interface-v1.json"
    return json.loads(path.read_text(encoding="utf-8"))


class AgentContractTests(unittest.TestCase):
    def test_shared_fixture_matches_python_surface(self):
        value = fixture()
        agent = define_agent(_agent_definition_from_wire(value["sessionSpec"]["agent"]))

        self.assertEqual(agent.name, "builder")
        self.assertEqual(to_wire(agent), value["sessionSpec"]["agent"])
        self.assertEqual(value["sessionSpec"]["models"][0]["protocol"], "responses")
        self.assertEqual(value["runSpec"]["limits"]["maxActiveAgents"], 4)

    def test_define_agent_rejects_invalid_budgets(self):
        agent = AgentDefinition(
            name="builder",
            instructions=(TextBlock("Build."),),
            model="primary",
            tools=(),
            budget=AgentBudget(0, 0, 1, 1),
        )

        with self.assertRaises(MuzenError) as caught:
            define_agent(agent)

        self.assertEqual(caught.exception.code, "invalid_input")
        self.assertEqual(caught.exception.details, {"path": "budget.max_turns"})

    def test_plain_string_inputs_normalize_to_one_text_block(self):
        self.assertEqual(
            normalize_agent_input("hello"),
            AgentInput(content=(TextBlock("hello"),)),
        )

    def test_integer_validation_rejects_floats(self):
        agent = AgentDefinition(
            name="builder",
            instructions=(TextBlock("Build."),),
            model="primary",
            tools=(),
            budget=AgentBudget(2.5, 0, 1, 1),
        )
        with self.assertRaises(MuzenError):
            define_agent(agent)

    def test_secret_input_repr_is_redacted(self):
        secret = PutSecretInput(value="c2VjcmV0")
        self.assertNotIn("c2VjcmV0", repr(secret))
        self.assertEqual(to_wire(secret), {"value": "c2VjcmV0"})
