export function sendReceiptSms(phone: string, totalCents: number): string {
  return `${phone}:${totalCents}`;
}
