import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";

import { MuzenError, type JsonObject } from "./agent.js";

const TOOL_NAME = /^[a-zA-Z0-9_-]{1,64}$/;
const SERVER_NAME = "muzen-python-tools";

export interface ToolInputSchema extends JsonObject {
  type: "object";
  properties: Record<string, unknown>;
  required?: string[];
  additionalProperties: false;
}

export interface Tool<TArguments extends object = JsonObject> {
  readonly name: string;
  readonly description: string;
  readonly input: ToolInputSchema;
  readonly execute: (arguments_: TArguments) => unknown | Promise<unknown>;
}

export interface FunctionToolOptions {
  readonly description: string;
  readonly input: ToolInputSchema;
}

export function tool<TArguments extends object>(definition: Tool<TArguments>): Tool<TArguments>;
export function tool<TArguments extends object>(
  execute: (arguments_: TArguments) => unknown | Promise<unknown>,
  options: FunctionToolOptions,
): Tool<TArguments>;
export function tool<TArguments extends object>(
  definitionOrExecute: Tool<TArguments> | ((arguments_: TArguments) => unknown | Promise<unknown>),
  options?: FunctionToolOptions,
): Tool<TArguments> {
  if (typeof definitionOrExecute !== "function" && !isObject(definitionOrExecute)) {
    throw invalid("tool", "must be an object or function");
  }
  const definition = typeof definitionOrExecute === "function"
    ? {
        name: definitionOrExecute.name,
        description: options?.description,
        input: options?.input,
        execute: definitionOrExecute,
      }
    : definitionOrExecute;

  if (typeof definition.name !== "string" || !TOOL_NAME.test(definition.name)) {
    throw invalid("tool.name", "must match [a-zA-Z0-9_-]{1,64}");
  }
  if (typeof definition.description !== "string") {
    throw invalid(`tool.${definition.name}.description`, "must be a string");
  }
  validateInputSchema(definition.input, `tool.${definition.name}.input`);
  if (typeof definition.execute !== "function") {
    throw invalid(`tool.${definition.name}.execute`, "must be a function");
  }

  return Object.freeze({
    name: definition.name,
    description: definition.description,
    input: definition.input,
    execute: definition.execute,
  });
}

export class LoopbackToolServer {
  private readonly tools: Map<string, Tool<any>>;
  private server: Server | undefined;
  private startPromise: Promise<void> | undefined;

  constructor(tools: readonly Tool<any>[]) {
    this.tools = new Map(tools.map((item) => [item.name, item]));
  }

  get url(): string {
    const address = this.server?.address();
    if (address === null || address === undefined || typeof address === "string") {
      throw new Error("tool server has not been started");
    }
    return `http://127.0.0.1:${address.port}/mcp`;
  }

  async start(): Promise<void> {
    if (this.server?.listening === true) return;
    this.startPromise ??= this.listen();
    try {
      await this.startPromise;
    } catch (error) {
      this.startPromise = undefined;
      this.server = undefined;
      throw error;
    }
  }

  async close(): Promise<void> {
    if (this.startPromise !== undefined) {
      try {
        await this.startPromise;
      } catch {
        // A failed start has no listening server to close.
      }
    }
    const server = this.server;
    this.server = undefined;
    this.startPromise = undefined;
    if (server === undefined || !server.listening) return;
    await new Promise<void>((resolve, reject) => {
      server.close((error) => error === undefined ? resolve() : reject(error));
      server.closeIdleConnections();
    });
  }

  private async listen(): Promise<void> {
    const server = createServer((request, response) => void this.handle(request, response));
    this.server = server;
    await new Promise<void>((resolve, reject) => {
      const onError = (error: Error): void => {
        server.off("listening", onListening);
        reject(error);
      };
      const onListening = (): void => {
        server.off("error", onError);
        resolve();
      };
      server.once("error", onError);
      server.once("listening", onListening);
      server.listen(0, "127.0.0.1");
    });
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    if (request.method !== "POST" || request.url !== "/mcp") {
      response.writeHead(404);
      response.end();
      return;
    }

    let payload: unknown;
    try {
      payload = JSON.parse(await readBody(request)) as unknown;
    } catch {
      writeError(response, null, -32700, "invalid JSON");
      return;
    }
    if (!isObject(payload)) {
      writeError(response, null, -32700, "invalid JSON");
      return;
    }

    const requestId = payload.id ?? null;
    const method = payload.method;
    if (method === "notifications/initialized") {
      response.writeHead(202, { "Content-Length": "0" });
      response.end();
      return;
    }
    if (method === "initialize") {
      writeResult(response, requestId, {
        protocolVersion: "2025-03-26",
        capabilities: { tools: {} },
        serverInfo: { name: SERVER_NAME, version: "1" },
      }, SERVER_NAME);
      return;
    }
    if (method === "tools/list") {
      writeResult(response, requestId, {
        tools: [...this.tools.values()].map((item) => ({
          name: item.name,
          description: item.description,
          inputSchema: item.input,
        })),
      });
      return;
    }
    if (method === "tools/call") {
      const params = isObject(payload.params) ? payload.params : {};
      const selected = typeof params.name === "string" ? this.tools.get(params.name) : undefined;
      if (selected === undefined) {
        writeError(response, requestId, -32602, "unknown tool");
        return;
      }
      const arguments_ = params.arguments === undefined ? {} : params.arguments;
      if (!isObject(arguments_)) {
        writeError(response, requestId, -32602, "arguments must be an object");
        return;
      }
      try {
        const value = await selected.execute(arguments_);
        const result: JsonObject = {
          content: [{ type: "text", text: typeof value === "string" ? value : pythonJsonStringify(value) }],
          isError: false,
        };
        if (typeof value !== "string") result.structuredContent = value;
        writeResult(response, requestId, result);
      } catch (error) {
        writeResult(response, requestId, {
          content: [{ type: "text", text: errorText(error) }],
          isError: true,
        });
      }
      return;
    }
    writeError(response, requestId, -32601, "method not found");
  }
}

function validateInputSchema(value: unknown, path: string): asserts value is ToolInputSchema {
  if (!isObject(value)) {
    throw invalid(path, "must be an object JSON Schema with properties and additionalProperties false");
  }
  const properties = value.properties;
  if (value.type !== "object" || !isObject(properties) || value.additionalProperties !== false) {
    throw invalid(path, "must be an object JSON Schema with properties and additionalProperties false");
  }
  if (
    value.required !== undefined &&
    (!Array.isArray(value.required) || value.required.some((item) => typeof item !== "string"))
  ) {
    throw invalid(`${path}.required`, "must be an array of property names");
  }
  if (
    Array.isArray(value.required) &&
    value.required.some((item) => typeof item === "string" && !(item in properties))
  ) {
    throw invalid(`${path}.required`, "must contain only declared property names");
  }
}

async function readBody(request: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  return Buffer.concat(chunks).toString("utf8");
}

function writeResult(response: ServerResponse, requestId: unknown, result: unknown, sessionId?: string): void {
  writeJson(response, { jsonrpc: "2.0", id: requestId, result }, sessionId);
}

function writeError(response: ServerResponse, requestId: unknown, code: number, message: string): void {
  writeJson(response, { jsonrpc: "2.0", id: requestId, error: { code, message } });
}

function writeJson(response: ServerResponse, value: unknown, sessionId?: string): void {
  const body = JSON.stringify(value);
  response.writeHead(200, {
    "Content-Type": "application/json",
    "Content-Length": String(Buffer.byteLength(body)),
    ...(sessionId === undefined ? {} : { "Mcp-Session-Id": sessionId }),
  });
  response.end(body);
}

function pythonJsonStringify(value: unknown): string {
  const compact = JSON.stringify(value);
  if (compact === undefined) throw new TypeError("Object of type undefined is not JSON serializable");
  let result = "";
  let quoted = false;
  let escaped = false;
  for (const character of compact) {
    result += character;
    if (quoted) {
      if (escaped) escaped = false;
      else if (character === "\\") escaped = true;
      else if (character === '"') quoted = false;
    } else if (character === '"') {
      quoted = true;
    } else if (character === "," || character === ":") {
      result += " ";
    }
  }
  return result;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function invalid(path: string, message: string): MuzenError {
  return new MuzenError("invalid_input", `${path} ${message}`, false, { path });
}
