// Spawn a swarm of agents over one repo snapshot, each free to bring its own
// model endpoint, and collect their final outputs.
//
//   MUZEN_RUNNER_PATH=target/debug/muzen-runner \
//   ANTHROPIC_API_KEY=... npx tsx examples/typescript/swarm/index.ts .
import { anthropic, createMuzen, openai, type SwarmAgent } from "@muzen/sdk";

const runnerPath = process.env.MUZEN_RUNNER_PATH;
const repo = process.argv[2] ?? ".";

// The run-level model is the default; any agent can override it with its own
// provider, base URL, and credential (vLLM, Ollama, a proxy, ...).
const hostedDefault = anthropic({ model: "claude-opus-4-8" });
const localEndpoint = openai({
  model: "qwen3-coder",
  baseUrl: process.env.LOCAL_MODEL_BASE_URL ?? "http://127.0.0.1:8000/v1",
  credential: { env: "LOCAL_MODEL_API_KEY" },
});

const objectives = [
  "Summarize the build configuration.",
  "List the public entry points.",
  "Describe the error-handling strategy.",
  "Identify the heaviest dependencies.",
];

const agents: SwarmAgent[] = objectives.map((objective, index) => ({
  id: `agent-${index}`,
  objective,
  // Alternate agents between the hosted default and the local endpoint.
  model: index % 2 === 0 ? undefined : localEndpoint,
  budget: {
    maxTurns: 6,
    maxToolCalls: 12,
    maxPromptTokens: 64_000,
    maxOutputTokens: 2_048,
  },
}));

const muzen = await createMuzen({ runnerPath });

try {
  const result = await muzen.runSwarm({
    repo,
    model: hostedDefault,
    agents,
  });

  console.log(`run ${result.runId}: ${result.status}`);
  console.log(
    `${result.usage.completedAgents}/${result.usage.agents} agents, ` +
      `${result.usage.modelCalls} model calls, ${result.usage.toolCalls} tool calls, ` +
      `${result.usage.totalTokens} tokens`,
  );
  for (const output of result.outputs) {
    console.log(`\n--- ${output.agentId} (${output.status}) ---`);
    console.log(output.output ?? "(no output)");
  }
} finally {
  await muzen.close();
}
