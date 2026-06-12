export type RetryDecision = {
  allowed: boolean
  delayMs: number
}

export function decideRetry(attempt: number, maxAttempts: number): RetryDecision {
  if (attempt >= maxAttempts) {
    return {
      allowed: false,
      delayMs: 0,
    }
  }

  return {
    allowed: true,
    delayMs: Math.min(1000 * attempt, 5000),
  }
}
