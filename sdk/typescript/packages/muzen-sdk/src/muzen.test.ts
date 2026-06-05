import { after, describe, it } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  createMuzen,
  local,
  MuzenUnsupportedFeatureError,
  type Muzen,
} from "./index.js";

const runnerPath = process.env.MUZEN_RUNNER_PATH;
const tempDirs: string[] = [];
let muzen: Muzen | undefined;

after(async () => {
  await muzen?.close();
  await Promise.all(
    tempDirs.map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

describe("runner-backed Muzen preview", () => {
  it(
    "runs a local review, replays events, and waits for a result",
    { skip: runnerPath ? false : "MUZEN_RUNNER_PATH is not set" },
    async () => {
      const repo = await mkdtemp(join(tmpdir(), "muzen-sdk-"));
      tempDirs.push(repo);
      await writeFile(
        join(repo, "Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
      );
      muzen = await createMuzen({ runnerPath });

      const review = await muzen.review(
        local(repo, { changedFiles: ["Cargo.toml"] }),
        {
          sessions: [
            {
              id: "security",
              role: "security",
              objective: "Find security regressions",
            },
          ],
        },
      );
      const result = await review.wait();
      const replayed: string[] = [];
      review.subscribe((event) => replayed.push(event.type));

      assert.equal(review.status, "completed");
      assert.equal(result.status, "completed");
      assert.match(result.summary, /Review completed/);
      assert.ok(replayed.includes("session.completed"));
      assert.equal((await review.refresh()).id, review.id);
    },
  );

  it(
    "keeps provider-backed sources explicit until materialization exists",
    { skip: runnerPath ? false : "MUZEN_RUNNER_PATH is not set" },
    async () => {
      muzen ??= await createMuzen({ runnerPath });

      await assert.rejects(
        () => muzen!.review("github:maskdotdev/heimdaal#123"),
        MuzenUnsupportedFeatureError,
      );
    },
  );
});
