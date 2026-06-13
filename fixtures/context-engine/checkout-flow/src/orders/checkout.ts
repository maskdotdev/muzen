import { applyDiscount } from "./discounts";
import { calculateTax } from "./tax";

export type CheckoutLine = {
  sku: string;
  quantity: number;
  unitPriceCents: number;
};

export type CheckoutSession = {
  subtotalCents: number;
  discountCents: number;
  taxCents: number;
  totalCents: number;
};

export function calculateOrderTotal(
  lines: CheckoutLine[],
  discountCode: string | null,
): CheckoutSession {
  const subtotalCents = lines.reduce(
    (total, line) => total + line.quantity * line.unitPriceCents,
    0,
  );
  const discountCents = applyDiscount(subtotalCents, discountCode);
  const taxableCents = subtotalCents - discountCents;
  const taxCents = calculateTax(taxableCents);

  return {
    subtotalCents,
    discountCents,
    taxCents,
    totalCents: taxableCents + taxCents,
  };
}

export function buildCheckoutSession(
  lines: CheckoutLine[],
  discountCode: string | null,
): CheckoutSession {
  return calculateOrderTotal(lines, discountCode);
}
