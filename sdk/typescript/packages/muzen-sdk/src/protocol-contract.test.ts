import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

interface RunnerPayloadRef {
  name: string;
}

interface RunnerMethodSchema {
  method: string;
  params?: RunnerPayloadRef;
  result?: RunnerPayloadRef;
}

interface RunnerPayloadFieldSchema {
  name: string;
  type: string;
  required: boolean;
  default?: string;
}

interface RunnerPayloadSchema {
  name: string;
  fields?: RunnerPayloadFieldSchema[];
  values?: string[];
}

interface RunnerProtocolSchema {
  schemaVersion: string;
  requests: RunnerMethodSchema[];
  callbacks: RunnerMethodSchema[];
  notifications: RunnerMethodSchema[];
  definitions: RunnerPayloadSchema[];
}

describe("runner protocol fixture", () => {
  it("documents the SDK wire shapes used by local runner adapters", async () => {
    const schema = await loadRunnerSchemaFixture();
    const definitions = new Map(
      schema.definitions.map((definition) => [definition.name, definition]),
    );

    assert.equal(schema.schemaVersion, "muzen.runner.v1");
    for (const method of [
      ...schema.requests,
      ...schema.callbacks,
      ...schema.notifications,
    ]) {
      for (const payload of [method.params, method.result]) {
        if (payload) {
          assert.ok(
            definitions.has(payload.name),
            `${method.method} references missing payload definition ${payload.name}`,
          );
        }
      }
    }

    const runStart = requireMethod(schema.requests, "run.start");
    assert.equal(runStart.params?.name, "RunStartParams");
    assert.equal(runStart.result?.name, "RunnerRunResult");
    assert.equal(
      requireField(definitions, "RunStartParams", "source").type,
      "ReviewSource",
    );
    assert.equal(
      requireField(definitions, "RunStartParams", "changedFiles").default,
      "[]",
    );

    assert.equal(
      requireMethod(schema.callbacks, "model.complete").params?.name,
      "RunnerModelCompleteParams",
    );
    assert.equal(
      requireMethod(schema.callbacks, "tool.execute").result?.name,
      "RunnerToolExecuteResult",
    );
    assert.equal(
      requireMethod(schema.notifications, "run.failed").params?.name,
      "RunFailedNotification",
    );
  });
});

async function loadRunnerSchemaFixture(): Promise<RunnerProtocolSchema> {
  const fixturePath = join(
    process.cwd(),
    "../../../../fixtures/runner-schema-v1.json",
  );
  return JSON.parse(await readFile(fixturePath, "utf8")) as RunnerProtocolSchema;
}

function requireMethod(
  methods: RunnerMethodSchema[],
  methodName: string,
): RunnerMethodSchema {
  const method = methods.find((candidate) => candidate.method === methodName);
  assert.ok(method, `missing method ${methodName}`);
  return method;
}

function requireField(
  definitions: Map<string, RunnerPayloadSchema>,
  definitionName: string,
  fieldName: string,
): RunnerPayloadFieldSchema {
  const definition = definitions.get(definitionName);
  assert.ok(definition, `missing definition ${definitionName}`);
  const field = definition.fields?.find((candidate) => candidate.name === fieldName);
  assert.ok(field, `missing field ${definitionName}.${fieldName}`);
  return field;
}
