import { describe, it } from "node:test";
import assert from "node:assert/strict";

import {
  projectReviewProgress,
  projectReviewProgressTimeline,
  type ReviewEvent,
} from "./index.js";

const baseEvent = {
  cursor: "1",
  reviewId: "review-1",
  timestampUtc: "2026-06-05T00:00:00Z",
  payload: {},
} satisfies Omit<ReviewEvent, "type">;

describe("review progress projection", () => {
  it("projects review events into stable progress phases", () => {
    const progress = projectReviewProgress({
      ...baseEvent,
      type: "repo.materialized",
    });

    assert.deepEqual(progress, {
      reviewId: "review-1",
      cursor: "1",
      timestampUtc: "2026-06-05T00:00:00Z",
      phase: "materializing",
      stage: undefined,
      percent: 30,
      terminal: false,
      message: "Materializing repository snapshot.",
      eventType: "repo.materialized",
    });
  });

  it("projects the last event in a list as current progress", () => {
    const progress = projectReviewProgress([
      { ...baseEvent, cursor: "1", type: "session.queued" },
      { ...baseEvent, cursor: "2", type: "tool.completed" },
      { ...baseEvent, cursor: "3", type: "session.completed" },
    ]);

    assert.equal(progress.phase, "done");
    assert.equal(progress.percent, 100);
    assert.equal(progress.terminal, true);
    assert.equal(progress.cursor, "3");
  });

  it("supports host stage maps without baking host names into core events", () => {
    const progress = projectReviewProgress(
      {
        ...baseEvent,
        type: "tool.started",
      },
      {
        stageMap: {
          materializing: "chunking",
          analyzing: "analyzing",
          saving: "saving",
          done: "done",
          failed: "failed",
          cancelled: "failed",
        },
      },
    );

    assert.equal(progress.phase, "analyzing");
    assert.equal(progress.stage, "analyzing");
  });

  it("projects timelines without collapsing intermediate states", () => {
    const timeline = projectReviewProgressTimeline([
      { ...baseEvent, cursor: "1", type: "session.queued" },
      { ...baseEvent, cursor: "2", type: "plan.created" },
      { ...baseEvent, cursor: "3", type: "session.failed" },
    ]);

    assert.deepEqual(
      timeline.map((item) => [item.cursor, item.phase, item.terminal]),
      [
        ["1", "queued", false],
        ["2", "planning", false],
        ["3", "failed", true],
      ],
    );
  });

  it("rejects empty event lists", () => {
    assert.throws(() => projectReviewProgress([]), /empty event list/);
  });
});
