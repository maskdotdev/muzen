import { expect, test } from "bun:test"

import { cacheTtlSeconds } from "../src/runtime/cache-policy"

test("uses a short profile cache ttl", () => {
  expect(cacheTtlSeconds("profile")).toBe(60)
})
