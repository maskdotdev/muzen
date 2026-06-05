export class MuzenUnsupportedFeatureError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "MuzenUnsupportedFeatureError";
  }
}
