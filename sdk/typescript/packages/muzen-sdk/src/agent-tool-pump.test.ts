import assert from "node:assert/strict";
import test from "node:test";

import { pumpClientToolRun } from "./agent-facade.js";
import {
  MuzenError,
  type AgentEvent,
  type AnswerToolCallInput,
  type Muzen,
  type Run,
} from "./agent.js";
import { tool } from "./tools.js";

function requested(toolName: string): AgentEvent {
  return {
    runId: "run-1",
    sequence: 1,
    type: "tool.requested",
    timestamp: "2026-07-16T00:00:00Z",
    payload: {
      callId: "call-1",
      provider: "local_tools",
      tool: toolName,
      arguments: { query: "runtime" },
      timeoutMs: 120_000,
    },
  };
}

const terminal: AgentEvent = {
  runId: "run-1",
  sequence: 2,
  type: "run.completed",
  timestamp: "2026-07-16T00:00:01Z",
  payload: {},
};

function fakeRun(events: readonly AgentEvent[] = [requested("lookup"), terminal]): Run {
  return {
    id: "run-1",
    async *events(): AsyncIterable<AgentEvent> {
      yield* events;
    },
  } as unknown as Run;
}

function fakeClient(
  answers: AnswerToolCallInput[],
  error?: MuzenError,
): Muzen {
  return {
    async answerToolCall(_runId: string, input: AnswerToolCallInput): Promise<void> {
      answers.push(input);
      if (error !== undefined) throw error;
    },
  } as unknown as Muzen;
}

test("client tool pump posts a non-retryable error when a tool throws", async () => {
  const answers: AnswerToolCallInput[] = [];
  const lookup = tool({
    name: "lookup",
    description: "Look up a query.",
    input: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
      additionalProperties: false,
    },
    execute: () => { throw new Error("lookup exploded"); },
  });

  await pumpClientToolRun(fakeClient(answers), fakeRun(), [lookup], new AbortController().signal);

  assert.deepEqual(answers, [{
    callId: "call-1",
    outcome: { error: { message: "lookup exploded", retryable: false } },
  }]);
});

test("client tool pump posts an error naming an unknown tool", async () => {
  const answers: AnswerToolCallInput[] = [];

  await pumpClientToolRun(
    fakeClient(answers),
    fakeRun([requested("missing_tool"), terminal]),
    [],
    new AbortController().signal,
  );

  assert.deepEqual(answers, [{
    callId: "call-1",
    outcome: { error: { message: "unknown client tool: missing_tool", retryable: false } },
  }]);
});

for (const code of ["conflict", "not_found"] as const) {
  test(`client tool pump swallows benign ${code} answers`, async () => {
    const answers: AnswerToolCallInput[] = [];
    const lookup = tool({
      name: "lookup",
      description: "Look up a query.",
      input: {
        type: "object",
        properties: { query: { type: "string" } },
        required: ["query"],
        additionalProperties: false,
      },
      execute: () => ({ found: true }),
    });

    await pumpClientToolRun(
      fakeClient(answers, new MuzenError(code, code, false)),
      fakeRun(),
      [lookup],
      new AbortController().signal,
    );

    assert.deepEqual(answers[0], {
      callId: "call-1",
      outcome: { result: { found: true } },
    });
  });
}

test("client tool pump resumes with its cursor and does not re-execute a replayed call", async () => {
  const answers: AnswerToolCallInput[] = [];
  const afterValues: Array<number | undefined> = [];
  let streams = 0;
  let executions = 0;
  let answerAttempts = 0;
  const started: AgentEvent = {
    runId: "run-1",
    sequence: 1,
    type: "tool.started",
    timestamp: "2026-07-16T00:00:00Z",
    payload: {},
  };
  const replayedRequest = { ...requested("lookup"), sequence: 2 };
  const completed = { ...terminal, sequence: 3 };
  const run = {
    id: "run-1",
    async *events(options?: { after?: number }): AsyncIterable<AgentEvent> {
      afterValues.push(options?.after);
      streams += 1;
      if (streams === 1) {
        yield started;
        yield replayedRequest;
        return;
      }
      yield replayedRequest;
      yield completed;
    },
  } as unknown as Run;
  const client = {
    async answerToolCall(_runId: string, input: AnswerToolCallInput): Promise<void> {
      answers.push(input);
      answerAttempts += 1;
      if (answerAttempts === 1) throw new MuzenError("unavailable", "answer response was lost", true);
    },
  } as unknown as Muzen;
  const lookup = tool({
    name: "lookup",
    description: "Look up a query.",
    input: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
      additionalProperties: false,
    },
    execute: () => {
      executions += 1;
      return { found: true };
    },
  });

  await pumpClientToolRun(client, run, [lookup], new AbortController().signal);

  assert.equal(executions, 1);
  assert.deepEqual(afterValues, [undefined, 1]);
  assert.equal(answers.length, 2);
  assert.deepEqual(answers[1], answers[0]);
});
