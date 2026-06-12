# Spawn a swarm of agents over one repo snapshot, each free to bring its own
# model endpoint, and collect their final outputs.
#
#   MUZEN_RUNNER_PATH=target/debug/muzen-runner \
#   OPENAI_API_KEY=... python examples/python/swarm.py .
import asyncio
import os
import sys

from muzen import (
    Client,
    ReviewAgentBudget,
    ReviewModelCredential,
    SwarmAgent,
    SwarmOptions,
    openai,
)


async def main() -> None:
    repo = sys.argv[1] if len(sys.argv) > 1 else "."

    # The run-level model is the default; any agent can override it with its
    # own provider, base URL, and credential (vLLM, Ollama, a proxy, ...).
    hosted_default = openai(model="gpt-5.4-mini")
    local_endpoint = openai(
        model="qwen3-coder",
        base_url=os.environ.get("LOCAL_MODEL_BASE_URL", "http://127.0.0.1:8000/v1"),
        credential=ReviewModelCredential(env="LOCAL_MODEL_API_KEY"),
    )

    objectives = [
        "Summarize the build configuration.",
        "List the public entry points.",
        "Describe the error-handling strategy.",
        "Identify the heaviest dependencies.",
    ]
    agents = [
        SwarmAgent(
            id=f"agent-{index}",
            objective=objective,
            # Alternate agents between the hosted default and the local endpoint.
            model=None if index % 2 == 0 else local_endpoint,
            budget=ReviewAgentBudget(
                max_turns=6,
                max_tool_calls=12,
                max_prompt_tokens=64_000,
                max_output_tokens=2_048,
            ),
        )
        for index, objective in enumerate(objectives)
    ]

    client = await Client.create(runner_path=os.environ.get("MUZEN_RUNNER_PATH"))
    try:
        result = await client.run_swarm(
            SwarmOptions(agents=agents, repo=repo, model=hosted_default)
        )

        print(f"run {result.run_id}: {result.status}")
        print(
            f"{result.usage.completed_agents}/{result.usage.agents} agents, "
            f"{result.usage.model_calls} model calls, "
            f"{result.usage.tool_calls} tool calls, "
            f"{result.usage.total_tokens} tokens"
        )
        for output in result.outputs:
            print(f"\n--- {output.agent_id} ({output.status}) ---")
            print(output.output or "(no output)")
    finally:
        await client.close()


if __name__ == "__main__":
    asyncio.run(main())
