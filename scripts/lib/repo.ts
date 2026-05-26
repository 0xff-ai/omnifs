import { $, TOML } from "bun";
import { dirname, join, resolve } from "node:path";
import { cwd } from "node:process";

export class Repo {
  constructor(readonly root: string) {}

  static async discover(start = cwd()): Promise<Repo> {
    return new Repo(await findRepoRoot(start));
  }

  path(...parts: string[]): string {
    return join(this.root, ...parts);
  }

  $(strings: TemplateStringsArray, ...expressions: unknown[]) {
    return $(strings, ...expressions).cwd(this.root);
  }

  async readJson<T>(...parts: string[]): Promise<T> {
    return await readJson<T>(this.path(...parts));
  }

  async readToml<T>(...parts: string[]): Promise<T> {
    return await readToml<T>(this.path(...parts));
  }

  async workspaceVersion(): Promise<string> {
    const cargo = await this.readToml<{ workspace?: { package?: { version?: string } } }>("Cargo.toml");
    const version = cargo.workspace?.package?.version;
    if (!version) {
      throw new Error("Cargo.toml missing [workspace.package].version");
    }
    return version;
  }
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function printErrorsAndExit(label: string, errors: string[]): never {
  for (const error of errors) {
    console.error(`error: ${error}`);
  }
  console.error(`${label} failed with ${errors.length} error(s)`);
  process.exit(1);
}

async function findRepoRoot(start: string): Promise<string> {
  let dir = resolve(start);
  while (true) {
    if (await isWorkspaceRoot(dir)) return dir;
    const parent = dirname(dir);
    if (parent === dir) {
      throw new Error("could not find repo root with [workspace] in Cargo.toml");
    }
    dir = parent;
  }
}

async function isWorkspaceRoot(dir: string): Promise<boolean> {
  const cargo = Bun.file(join(dir, "Cargo.toml"));
  if (!await cargo.exists()) return false;
  try {
    const parsed = TOML.parse(await cargo.text()) as { workspace?: unknown };
    return parsed.workspace !== undefined;
  } catch {
    return false;
  }
}

async function readJson<T>(path: string): Promise<T> {
  try {
    return await Bun.file(path).json() as T;
  } catch (error) {
    throw new Error(`parse ${path}: ${errorMessage(error)}`);
  }
}

async function readToml<T>(path: string): Promise<T> {
  try {
    return TOML.parse(await Bun.file(path).text()) as T;
  } catch (error) {
    throw new Error(`parse ${path}: ${errorMessage(error)}`);
  }
}
