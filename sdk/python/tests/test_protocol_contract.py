import json
import unittest
from pathlib import Path


class RunnerProtocolContractTests(unittest.TestCase):
    def test_fixture_documents_sdk_wire_shapes(self) -> None:
        schema = _load_runner_schema_fixture()
        definitions = {definition["name"]: definition for definition in schema["definitions"]}

        self.assertEqual(schema["schemaVersion"], "muzen.runner.v1")
        for method in (
            schema["requests"] + schema["callbacks"] + schema["notifications"]
        ):
            for payload in (method.get("params"), method.get("result")):
                if payload is not None:
                    self.assertIn(
                        payload["name"],
                        definitions,
                        f"{method['method']} references missing payload definition {payload['name']}",
                    )

        run_start = _require_method(schema["requests"], "run.start")
        self.assertEqual(run_start["params"]["name"], "RunStartParams")
        self.assertEqual(run_start["result"]["name"], "RunnerRunResult")
        self.assertEqual(
            _require_field(definitions, "RunStartParams", "source")["type"],
            "ReviewSource",
        )
        self.assertEqual(
            _require_field(definitions, "RunStartParams", "changedFiles")["default"],
            "[]",
        )

        self.assertEqual(
            _require_method(schema["callbacks"], "model.complete")["params"]["name"],
            "RunnerModelCompleteParams",
        )
        self.assertEqual(
            _require_method(schema["callbacks"], "secret.resolve")["result"]["name"],
            "RunnerSecretResolveResult",
        )
        self.assertEqual(
            _require_method(schema["callbacks"], "tool.execute")["result"]["name"],
            "RunnerToolExecuteResult",
        )
        self.assertEqual(
            _require_method(schema["notifications"], "run.failed")["params"]["name"],
            "RunFailedNotification",
        )


def _load_runner_schema_fixture():
    fixture_path = (
        Path(__file__).resolve().parents[3] / "fixtures" / "runner-schema-v1.json"
    )
    return json.loads(fixture_path.read_text(encoding="utf-8"))


def _require_method(methods, method_name):
    for method in methods:
        if method["method"] == method_name:
            return method
    raise AssertionError(f"missing method {method_name}")


def _require_field(definitions, definition_name, field_name):
    definition = definitions.get(definition_name)
    if definition is None:
        raise AssertionError(f"missing definition {definition_name}")
    for field in definition.get("fields", []):
        if field["name"] == field_name:
            return field
    raise AssertionError(f"missing field {definition_name}.{field_name}")


if __name__ == "__main__":
    unittest.main()
