import { RunnerBackedMuzen } from "./local.js";
import { RunnerStdioClient } from "./protocol.js";
import { RemoteMuzen } from "./remote.js";
import type {
  CreateMuzenClientOptions,
  CreateMuzenOptions,
  CreateReviewSessionInput,
  CreateReviewSessionResult,
  Muzen,
} from "./types.js";

export { MuzenUnsupportedFeatureError } from "./errors.js";

export async function createMuzen(
  options: CreateMuzenOptions = {},
): Promise<Muzen> {
  const runner = new RunnerStdioClient({
    runnerPath:
      options.runnerPath ?? process.env.MUZEN_RUNNER_PATH ?? "muzen-runner",
    runnerArgs: options.runnerArgs ?? ["stdio"],
  });
  await runner.handshake({
    clientName: options.clientName ?? "@muzen/sdk",
    clientVersion: options.clientVersion,
  });
  return new RunnerBackedMuzen(runner);
}

export function createMuzenClient(
  options: CreateMuzenClientOptions,
): Muzen {
  return new RemoteMuzen(options);
}

export async function createReviewSession(
  input: CreateReviewSessionInput,
): Promise<CreateReviewSessionResult> {
  const muzen = await createMuzen(input.muzen);
  const review = await muzen.review(input.source, input.options);
  return { muzen, review };
}
