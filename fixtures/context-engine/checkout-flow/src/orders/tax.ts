export function calculateTax(taxableCents: number): number {
  return Math.floor(taxableCents * 0.0825);
}
