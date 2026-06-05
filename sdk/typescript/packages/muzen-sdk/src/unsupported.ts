import { MuzenUnsupportedFeatureError } from "./errors.js";
import type {
  MuzenWorkerRun,
  MuzenWorkerRunOnceOptions,
  MuzenWorkers,
  MuzenWorkerStartOptions,
  WorkspaceProfileCollection,
} from "./types.js";

export class UnsupportedMuzenWorkers implements MuzenWorkers {
  constructor(private readonly message: string) {}

  runOnce(_options: MuzenWorkerRunOnceOptions = {}): Promise<MuzenWorkerRun> {
    return Promise.reject(new MuzenUnsupportedFeatureError(this.message));
  }

  start(_options: MuzenWorkerStartOptions = {}): Promise<void> {
    return Promise.reject(new MuzenUnsupportedFeatureError(this.message));
  }
}

export class UnsupportedWorkspaceProfileCollection<Input, Profile>
  implements WorkspaceProfileCollection<Input, Profile>
{
  constructor(private readonly kind: string) {}

  set(_name: string, _input: Input): Promise<Profile> {
    return Promise.reject(this.error());
  }

  get(_name: string): Promise<Profile | undefined> {
    return Promise.reject(this.error());
  }

  list(): Promise<Profile[]> {
    return Promise.reject(this.error());
  }

  private error(): MuzenUnsupportedFeatureError {
    return new MuzenUnsupportedFeatureError(
      `workspace ${this.kind} profiles require remote workspace storage; createMuzen() only supports local runner review execution in this preview`,
    );
  }
}
