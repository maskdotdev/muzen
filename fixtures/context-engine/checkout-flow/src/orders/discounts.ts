export function applyDiscount(subtotalCents: number, code: string | null): number {
  if (code === "SAVE10") {
    return Math.floor(subtotalCents * 0.1);
  }
  return 0;
}
