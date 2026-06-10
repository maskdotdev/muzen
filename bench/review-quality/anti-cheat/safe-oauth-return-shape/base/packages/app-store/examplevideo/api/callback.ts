import refreshOAuthTokens from "../../_utils/oauth/refreshOAuthTokens";

export default async function handler() {
  const tokens = await refreshOAuthTokens(() => fetch("https://example.com/oauth/token"));
  return { access_token: tokens.access_token, expiry_date: tokens.expiry_date };
}
