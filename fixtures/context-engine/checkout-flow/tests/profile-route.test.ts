import { profileRoute } from "../src/api/profile-route";

test("profile route returns user", () => {
  expect(profileRoute().body.id).toBe("user-1");
});
