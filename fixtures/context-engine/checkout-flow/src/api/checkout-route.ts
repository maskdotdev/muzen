import { buildCheckoutSession, type CheckoutLine } from "@checkout/orders/checkout";

export type CheckoutRequest = {
  lines: CheckoutLine[];
  discountCode?: string;
};

export function checkoutRoute(request: CheckoutRequest) {
  const session = buildCheckoutSession(request.lines, request.discountCode ?? null);
  return {
    status: 200,
    body: session,
  };
}
