import { inventoryRoute } from "../src/api/inventory-route";

test("inventory route returns stock", () => {
  expect(inventoryRoute().body[0].available).toBe(4);
});
