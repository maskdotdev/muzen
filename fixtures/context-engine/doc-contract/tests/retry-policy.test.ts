import { expect, test } from "bun:test"

import { decideRetry } from "../src/runtime/retry-policy"

test("blocks retries after the configured maximum", () => {
  expect(decideRetry(3, 3)).toEqual({
    allowed: false,
    delayMs: 0,
  })
})
