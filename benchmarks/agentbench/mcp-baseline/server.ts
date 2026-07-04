#!/usr/bin/env bun
// Structurally-honest MCP baseline for agentbench condition B.
//
// A stdio MCP server (newline-delimited JSON-RPC 2.0) exposing exactly two
// tools, `list_dir(path)` and `read_file(path)`, backed by the same
// `fixture-data/` directory that condition A serves through omnifs. This is the
// v1 baseline: the same bytes, reached through a tool-call interaction model
// instead of a filesystem. T4 upgrades condition B to real vendor MCP servers.
//
// Root is taken from FIXTURE_ROOT (env) or argv[2]. Every path is resolved and
// prefix-checked against the root, so traversal outside the corpus is rejected.

import { readdirSync, readFileSync, realpathSync, statSync } from "node:fs";
import { isAbsolute, join, resolve, sep } from "node:path";

const rawRoot = process.env.FIXTURE_ROOT ?? process.argv[2];
if (!rawRoot) {
  process.stderr.write("fixture MCP server: FIXTURE_ROOT is required\n");
  process.exit(1);
}
const ROOT = realpathSync(resolve(rawRoot));

// Resolve a client-supplied path against ROOT and confirm it stays inside.
function safeResolve(userPath: string): string {
  const rel = isAbsolute(userPath) ? userPath.replace(/^[/\\]+/, "") : userPath;
  const abs = resolve(join(ROOT, rel));
  if (abs !== ROOT && !abs.startsWith(ROOT + sep)) {
    throw new Error(`path escapes fixture root: ${userPath}`);
  }
  return abs;
}

interface JsonRpcRequest {
  jsonrpc: "2.0";
  id?: number | string | null;
  method: string;
  params?: Record<string, unknown>;
}

const TOOLS = [
  {
    name: "list_dir",
    description:
      "List the entries of a directory within the dataset. `path` is relative to the dataset root; use \"\" or \".\" for the root.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", description: "Directory path relative to the dataset root" },
      },
      required: ["path"],
    },
  },
  {
    name: "read_file",
    description:
      "Read the full contents of a file within the dataset. `path` is relative to the dataset root.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", description: "File path relative to the dataset root" },
      },
      required: ["path"],
    },
  },
];

function listDir(path: string): string {
  const abs = safeResolve(path);
  const entries = readdirSync(abs, { withFileTypes: true })
    .map((e) => (e.isDirectory() ? `${e.name}/` : e.name))
    .sort();
  return entries.length ? entries.join("\n") : "(empty directory)";
}

function readFile(path: string): string {
  const abs = safeResolve(path);
  if (statSync(abs).isDirectory()) {
    throw new Error(`not a file: ${path}`);
  }
  return readFileSync(abs, "utf8");
}

function send(msg: unknown): void {
  process.stdout.write(`${JSON.stringify(msg)}\n`);
}

function handle(req: JsonRpcRequest): void {
  const { id, method, params } = req;

  // Notifications (no id) get no response.
  const isNotification = id === undefined || id === null;

  try {
    switch (method) {
      case "initialize": {
        const clientVersion =
          (params?.protocolVersion as string | undefined) ?? "2025-06-18";
        send({
          jsonrpc: "2.0",
          id,
          result: {
            protocolVersion: clientVersion,
            capabilities: { tools: {} },
            serverInfo: { name: "agentbench-fixture", version: "0.1.0" },
          },
        });
        return;
      }
      case "notifications/initialized":
        return; // notification, no reply
      case "ping":
        if (!isNotification) send({ jsonrpc: "2.0", id, result: {} });
        return;
      case "tools/list":
        send({ jsonrpc: "2.0", id, result: { tools: TOOLS } });
        return;
      case "tools/call": {
        const name = params?.name as string;
        const args = (params?.arguments as Record<string, unknown>) ?? {};
        const path = String(args.path ?? "");
        let text: string;
        if (name === "list_dir") text = listDir(path);
        else if (name === "read_file") text = readFile(path);
        else throw new Error(`unknown tool: ${name}`);
        send({
          jsonrpc: "2.0",
          id,
          result: { content: [{ type: "text", text }] },
        });
        return;
      }
      default:
        if (!isNotification) {
          send({
            jsonrpc: "2.0",
            id,
            error: { code: -32601, message: `method not found: ${method}` },
          });
        }
        return;
    }
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    if (method === "tools/call" && !isNotification) {
      // Tool errors are returned in-band per MCP so the model can react.
      send({
        jsonrpc: "2.0",
        id,
        result: { content: [{ type: "text", text: `error: ${message}` }], isError: true },
      });
    } else if (!isNotification) {
      send({ jsonrpc: "2.0", id, error: { code: -32603, message } });
    }
  }
}

// Read newline-delimited JSON-RPC from stdin.
let buffer = "";
const decoder = new TextDecoder();
for await (const chunk of Bun.stdin.stream()) {
  buffer += decoder.decode(chunk, { stream: true });
  let nl: number;
  while ((nl = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, nl).trim();
    buffer = buffer.slice(nl + 1);
    if (!line) continue;
    try {
      handle(JSON.parse(line) as JsonRpcRequest);
    } catch {
      // Ignore unparseable lines rather than crash the transport.
    }
  }
}
