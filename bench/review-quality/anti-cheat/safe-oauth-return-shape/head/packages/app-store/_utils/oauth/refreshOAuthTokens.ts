const refreshOAuthTokens = async (refreshFunction: () => Promise<Response>, expiresInSeconds?: number) => {
  const response = await refreshFunction();
  const tokens = await response.json();
  return { ...tokens, expiry_date: Date.now() + (expiresInSeconds ?? tokens.expires_in) * 1000 };
};

export default refreshOAuthTokens;
