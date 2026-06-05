import { createMuzen, local } from "@muzen/sdk";

const runnerPath = process.env.MUZEN_RUNNER_PATH;
const repo = process.argv[2] ?? ".";
const changedFiles = process.argv.slice(3);

const muzen = await createMuzen({ runnerPath });

try {
  const review = await muzen.review(
    local(repo, {
      changedFiles,
    }),
  );

  review.subscribe((event) => {
    console.log(event.type);
  });

  const result = await review.wait();
  console.log(result.conclusion);
  console.log(result.summary);
} finally {
  await muzen.close();
}
