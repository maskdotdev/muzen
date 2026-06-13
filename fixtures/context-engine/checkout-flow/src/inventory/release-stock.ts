export function releaseStock(reservationId: string): string {
  return `released:${reservationId}`;
}
