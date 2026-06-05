import { createMuzen, local, type ReviewEvent } from "@muzen/sdk";

const runnerPath = process.env.MUZEN_RUNNER_PATH;
const repo = process.argv[2] ?? ".";
const changedFiles = process.argv.slice(3);
const after = process.env.MUZEN_AFTER_CURSOR ?? null;

const muzen = await createMuzen({ runnerPath });

try {
  const review = await muzen.review(
    local(repo, {
      changedFiles,
    }),
  );

  let eventCount = 0;
  for await (const event of review.events({ after })) {
    eventCount += 1;
    printEvent(event);
  }

  const result = await review.wait();
  console.log(`review ${review.id} replayed ${eventCount} event(s)`);
  console.log(result.conclusion);
  console.log(result.summary);
} finally {
  await muzen.close();
}

function printEvent(event: ReviewEvent): void {
  const prefix = `${event.cursor} ${event.type}`;
  if (event.payload === undefined || event.payload === null) {
    console.log(prefix);
    return;
  }
  console.log(`${prefix} ${JSON.stringify(event.payload)}`);
}
