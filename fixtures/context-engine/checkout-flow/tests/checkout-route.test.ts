import { checkoutRoute } from "../src/api/checkout-route";

test("checkout route returns discounted taxed total", () => {
  const response = checkoutRoute({
    discountCode: "SAVE10",
    lines: [{ sku: "sku-1", quantity: 2, unitPriceCents: 1000 }],
  });

  expect(response.body.totalCents).toBe(1965);
});
