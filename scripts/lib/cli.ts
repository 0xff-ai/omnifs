import { parseArgs as parseNodeArgs, type ParseArgsConfig } from "node:util";
import { errorMessage } from "./repo";

type Options = NonNullable<ParseArgsConfig["options"]>;

export function parseArgs<O extends Options>(args: string[], options: O) {
  return parseNodeArgs({ args, allowPositionals: true, options });
}

export function takeCommandWithVersion(
  values: Record<string, unknown>,
  positionals: string[],
): { command: string; version: string | undefined } {
  const [command = "", positionalVersion, ...rest] = positionals;
  if (rest.length > 0) throw new Error(`unexpected argument: ${rest[0]}`);
  const flagVersion = typeof values.version === "string" ? values.version : undefined;
  if (flagVersion && positionalVersion) {
    throw new Error("pass version as a positional or --version, not both");
  }
  return { command, version: flagVersion ?? positionalVersion };
}

export async function runCli(main: () => Promise<void>): Promise<void> {
  try {
    await main();
  } catch (error) {
    console.error(errorMessage(error));
    process.exit(1);
  }
}
