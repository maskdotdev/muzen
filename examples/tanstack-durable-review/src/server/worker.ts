import {
  createMuzen,
  type ReviewSource,
} from "@muzen/sdk";

import { DurableReviewStore } from "./store.js";

export interface WorkerOptions {
  runnerPath?: string;
}

export async function executeReview(
  store: DurableReviewStore,
  reviewId: string,
  options: WorkerOptions = {},
): Promise<void> {
  const review = store.get(reviewId);
  store.markRunning(reviewId);
  let muzen: Awaited<ReturnType<typeof createMuzen>> | undefined;

  try {
    muzen = await createMuzen({
      runnerPath: options.runnerPath,
      clientName: "tanstack-durable-review-example",
    });
    const appended = new Set<string>();
    const session = await muzen.review(
      review.source as ReviewSource,
      {
        ...review.options,
        hooks: {
          ...review.options.hooks,
          onEvent: (event) => {
            if (!appended.has(event.cursor)) {
              appended.add(event.cursor);
              store.appendRunnerEvent(reviewId, event);
            }
            review.options.hooks?.onEvent?.(event);
          },
        },
      },
    );
    for await (const event of session.events()) {
      if (!appended.has(event.cursor)) {
        appended.add(event.cursor);
        store.appendRunnerEvent(reviewId, event);
      }
    }
    const result = await session.wait();
    store.complete(reviewId, result);
  } catch (error) {
    store.fail(reviewId, error instanceof Error ? error.message : String(error));
  } finally {
    await muzen?.close();
  }
}
