const refreshOAuthTokens = async (refreshFunction: () => Promise<Response>) => {
  const response = await refreshFunction();
  return response.json();
};

export default refreshOAuthTokens;
