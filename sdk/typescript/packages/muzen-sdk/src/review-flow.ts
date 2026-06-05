import type { ReviewResult } from "./types.js";

export function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw new Error("operation aborted");
  }
}

export function parseTimeoutMs(timeout: string | number | undefined): number | undefined {
  if (timeout === undefined) {
    return undefined;
  }
  if (typeof timeout === "number") {
    return timeout;
  }
  const match = timeout.trim().match(/^(\d+)(ms|s|m)?$/);
  if (!match) {
    throw new Error(`invalid timeout: ${timeout}`);
  }
  const amount = Number(match[1]);
  const unit = match[2] ?? "ms";
  switch (unit) {
    case "ms":
      return amount;
    case "s":
      return amount * 1000;
    case "m":
      return amount * 60_000;
  }
}

export function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`review wait timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    promise
      .then(resolve, reject)
      .finally(() => clearTimeout(timer));
  });
}

export async function pollUntilResult(
  load: () => Promise<ReviewResult | undefined>,
  signal: AbortSignal | undefined,
): Promise<ReviewResult> {
  while (true) {
    throwIfAborted(signal);
    const result = await load();
    if (result) {
      return result;
    }
    await delay(250, signal);
  }
}

export function delay(ms: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        reject(new Error("operation aborted"));
      },
      { once: true },
    );
  });
}
