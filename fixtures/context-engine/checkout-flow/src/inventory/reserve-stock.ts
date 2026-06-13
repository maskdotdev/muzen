export function reserveStock(sku: string, quantity: number): string {
  return `${sku}:${quantity}`;
}
