export function sendReceiptEmail(email: string, totalCents: number): string {
  return `${email}:${totalCents}`;
}
