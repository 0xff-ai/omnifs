#!/usr/bin/env bun

import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import { fileURLToPath } from "node:url";

type ShellOutput = { exitCode: number };
type ShellCommand = PromiseLike<ShellOutput> & {
  cwd(path: string): ShellCommand;
  env(env: Record<string, string | undefined>): ShellCommand;
  nothrow(): ShellCommand;
  quiet(): ShellCommand;
  text(): Promise<string>;
};
type ShellTag = (strings: TemplateStringsArray, ...values: unknown[]) => ShellCommand;

declare const Bun: {
  argv: string[];
  $: ShellTag;
  which(command: string): string | null;
  spawn(
    args: string[],
    options: {
      cwd: string;
      env: Record<string, string | undefined>;
      stdin: "inherit";
      stdout: "inherit";
      stderr: "inherit";
    },
  ): { exited: Promise<number> };
};

type DevOptions = {
  profile: string;
  filesystemImage: string | null;
  yes: boolean;
  detach: boolean;
  noShell: boolean;
  home: string | null;
  providerStore: string | null;
  skipCliBuild: boolean;
  buildOnly: boolean;
};

type ProviderStoreIndex = {
  version: 2;
  providers: Array<{ id: string; name: string; version?: string }>;
};

type DevMountTemplate = {
  mount: string;
  provider: string;
  auth?: {
    type?: string;
    scheme?: string;
  };
  config?: Record<string, unknown>;
  limits?: unknown;
};

type DevMountRender = {
  name: string;
  provider: string;
  template: DevMountTemplate;
  tokenEnv?: string;
};

type DevHomeRender = {
  mounts: DevMountRender[];
  skipped: string[];
  credentialEnv: Record<string, string>;
};

type TemplateEntry = {
  path: string;
  template: DevMountTemplate;
};

/// Host paths the db/k8s dev fixtures seed into, computed once from `devHome`
/// so the rendered mount config and the fixture containers agree on where the
/// data actually lands. See `renderDevHomePlan` and `startFixtures`.
type FixturePaths = {
  dbPath: string;
  k8sSockPath: string;
};

type Fixtures = {
  k8s: boolean;
  dbContainerId: string | null;
};

const $ = Bun.$;
const DB_IMAGE = "omnifs-dev-db:local";
const DB_CONTAINER = "omnifs-dev-db";
const K8S_COMPOSE_PROJECT = "omnifs-devcluster";
const FILESYSTEM_DEV_IMAGE = "omnifs-filesystem:dev";
// The fixed guest location in `omnifs_core::filesystem`; guest filesystems always mount here.
const GUEST_MOUNT = "/omnifs";
// The filesystem image ships a minimal Debian base (fuse3, coreutils, findutils,
// jq, rsync, tar, xxd) with no bash/zsh (`Dockerfile`'s `filesystem-base`), so
// the interactive dev shell and any container-side probe use POSIX `/bin/sh`.
const GUEST_SHELL = "/bin/sh";
// Dev mounts whose provider needs a static token, and the host env var that
// holds it. Dev orchestration, not provider or mount data, so it lives here
// rather than in the mount templates.
const DEV_TOKEN_ENV: Record<string, string> = { github: "GITHUB_TOKEN", linear: "LINEAR_API_KEY" };

const scriptDir = dirname(fileURLToPath(import.meta.url));
const workspace = resolve(scriptDir, "..");
process.chdir(workspace);

main().catch((error) => {
  console.error(`error: ${error.message}`);
  process.exit(1);
});

async function main() {
  const options = parseArgs(Bun.argv.slice(2));
  await checkPrerequisites(options);

  const devHome =
    options.home || process.env.OMNIFS_HOME || join(homedir(), ".omnifs-dev");
  const profileMounts = readProfile(options.profile);
  const filesystemImage = options.filesystemImage || `omnifs-filesystem:${await gitShortHead()}-dev`;
  const providerStore = resolve(
    options.providerStore || join(workspace, "target/omnifs-provider-store"),
  );

  console.log(`Workspace: ${workspace}`);
  if (!options.providerStore) {
    await run($`just build providers`);
  }
  assertFile(join(providerStore, "index.json"), "provider store bundle");

  const builds: Promise<void>[] = [];
  if (!options.skipCliBuild) {
    // The full CLI package includes the host-native daemon process owner and
    // NFS filesystem support used by mount and filesystem commands.
    builds.push(
      run($`cargo build -p omnifs-cli`.env({
        ...process.env,
        OMNIFS_PROVIDER_BUNDLE_DIR: providerStore,
      })),
    );
  }
  if (!options.filesystemImage) {
    builds.push(buildFilesystemImage(filesystemImage).then(() => tagFloatingFilesystemImage(filesystemImage)));
  }
  await Promise.all(builds);

  if (options.buildOnly) {
    const built = [];
    if (!options.skipCliBuild) {
      built.push("the omnifs CLI");
    }
    if (!options.filesystemImage) {
      const tags = filesystemImage === FILESYSTEM_DEV_IMAGE ? [filesystemImage] : [filesystemImage, FILESYSTEM_DEV_IMAGE];
      built.push(`the filesystem image (${tags.join(" and ")})`);
    }
    console.log(`✓ Built ${built.join(" and ")}`);
    return;
  }

  const omnifsCli = resolveCli();
  const fixturePaths = fixturePathsFor(devHome);

  const render = await renderDevHomePlan(profileMounts, providerStore, fixturePaths, options);
  if (render.mounts.length === 0) {
    throw new Error(`profile ${options.profile} rendered no usable mounts`);
  }

  if (!options.yes) {
    printPlan({
      devHome,
      filesystemImage,
      profile: options.profile,
      render,
      keepRunning: keepRunning(options),
    });
    const proceed = await confirm("Proceed?", true);
    if (!proceed) {
      throw new Error("aborted by user");
    }
  }

  const fixtures = await startFixtures(render.mounts, fixturePaths);
  try {
    // Host resources must exist before declarative apply wakes daemon
    // reconciliation.
    const hostProtocol = process.platform === "linux" ? "fuse" : "nfs";
    const hostLocation = join(devHome, "mnt");
    mkdirSync(hostLocation, { recursive: true });
    const configPath = writeDevConfig(
      devHome,
      providerStore,
      render,
      filesystemImage,
      hostProtocol,
      hostLocation,
    );
    const revision = await applyDevConfig(
      omnifsCli,
      configPath,
      cliEnv(devHome, { OMNIFS_FILESYSTEM_IMAGE: filesystemImage }),
      render.mounts.filter((mount) => mount.tokenEnv).map(credentialName),
    );
    for (const mount of render.mounts) {
      if (!mount.tokenEnv) {
        continue;
      }
      await run(
        $`${omnifsCli} credential set ${credentialName(mount)} --from-env ${mount.tokenEnv}`.env(
          cliEnv(devHome, render.credentialEnv),
        ),
      );
    }
    await run(
      $`${omnifsCli} status --follow --revision ${revision}`.env(
        cliEnv(devHome, { OMNIFS_FILESYSTEM_IMAGE: filesystemImage }),
      ),
    );

    if (keepRunning(options)) {
      if (options.detach) {
        console.log(`Detached. Stop with \`${omnifsCli} down\`.`);
      }
      return;
    }

    try {
      console.log(`Opening a shell in \`dev-docker\` at ${GUEST_MOUNT}`);
      await runInteractive(
        [omnifsCli, "fs", "shell", "dev-docker", "--", GUEST_SHELL],
        cliEnv(devHome, { OMNIFS_FILESYSTEM_IMAGE: filesystemImage }),
      );
    } finally {
      await teardownSession(devHome, omnifsCli, fixturePaths, fixtures);
    }
  } catch (error) {
    await teardownSession(devHome, omnifsCli, fixturePaths, fixtures);
    throw error;
  }
}

function parseArgs(args: string[]): DevOptions {
  const options: DevOptions = {
    profile: "default",
    filesystemImage: null,
    yes: false,
    detach: false,
    noShell: false,
    home: null,
    providerStore: null,
    skipCliBuild: false,
    buildOnly: false,
  };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "-y" || arg === "--yes" || arg === "/y") {
      options.yes = true;
    } else if (arg === "--profile") {
      options.profile = requireValue(args, ++i, "--profile");
    } else if (arg === "--filesystem-image") {
      options.filesystemImage = requireValue(args, ++i, "--filesystem-image");
    } else if (arg === "--home") {
      options.home = requireValue(args, ++i, "--home");
    } else if (arg === "--provider-store") {
      options.providerStore = requireValue(args, ++i, "--provider-store");
    } else if (arg === "--skip-cli-build") {
      options.skipCliBuild = true;
    } else if (arg === "--build-only") {
      options.buildOnly = true;
    } else if (arg === "--detach") {
      options.detach = true;
    } else if (arg === "--no-shell") {
      options.noShell = true;
    } else {
      throw new Error(`unknown argument ${arg}`);
    }
  }
  return options;
}

function requireValue(args: string[], index: number, flag: string): string {
  const value = args[index];
  if (!value || value.startsWith("--")) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}

async function checkPrerequisites(options: DevOptions): Promise<void> {
  const commands = ["bun", "docker"];
  if (!options.providerStore) {
    commands.push("just");
  }
  if (!options.skipCliBuild) {
    commands.push("cargo");
  }
  if (!options.filesystemImage) {
    commands.push("git");
  }

  for (const command of commands) {
    if (!commandExists(command)) {
      throw new Error(`missing prerequisite: ${command}`);
    }
  }
  if ((await $`docker info`.quiet().nothrow()).exitCode !== 0) {
    throw new Error("Docker daemon did not respond; start Docker and rerun");
  }
}

function commandExists(command: string): boolean {
  return Bun.which(command) !== null;
}

/// Resolve the exact binary this script drives. CI may provide a packaged CLI
/// through `OMNIFS_CLI`; contributor runs use the compiled worktree binary.
/// Neither path may fall back to a globally-installed shim implicitly.
function resolveCli(): string {
  if (process.env.OMNIFS_CLI) {
    const configured = resolve(process.env.OMNIFS_CLI);
    if (existsSync(configured)) {
      return configured;
    }
    throw new Error(`configured omnifs CLI does not exist at ${configured}`);
  }
  const built = join(workspace, "target/debug/omnifs");
  if (existsSync(built)) {
    return built;
  }
  throw new Error(
    `no compiled omnifs CLI found at ${built}: build one (drop --skip-cli-build)`,
  );
}

function readProfile(profile: string): string[] {
  const path = join(workspace, "contrib/dev-profiles", `${profile}.toml`);
  const raw = readFileSync(path, "utf8");
  const match = raw.match(/mounts\s*=\s*\[([^\]]*)\]/m);
  if (!match) {
    throw new Error(`profile ${path} does not define mounts = [...]`);
  }
  return [...match[1].matchAll(/"([^"]+)"/g)].map((item) => item[1]);
}

function discoverTemplates(): Map<string, TemplateEntry> {
  const providersDir = join(workspace, "providers");
  const templates = new Map<string, TemplateEntry>();
  for (const provider of readdirSync(providersDir)) {
    const path = join(providersDir, provider, "dev/mount.json");
    if (!existsSync(path)) {
      continue;
    }
    const template = JSON.parse(readFileSync(path, "utf8")) as DevMountTemplate;
    templates.set(template.mount, { path, template });
  }
  return templates;
}

function fixturePathsFor(devHome: string): FixturePaths {
  return {
    dbPath: join(devHome, "fixtures/db/test.db"),
    k8sSockPath: join(devHome, "fixtures/k8s/k8s.sock"),
  };
}

async function renderDevHomePlan(
  profileMounts: string[],
  providerStore: string,
  fixturePaths: FixturePaths,
  options: DevOptions,
): Promise<DevHomeRender> {
  const index = JSON.parse(readFileSync(join(providerStore, "index.json"), "utf8")) as ProviderStoreIndex;
  const templates = discoverTemplates();
  const mounts: DevMountRender[] = [];
  const skipped: string[] = [];
  const credentialEnv: Record<string, string> = {};

  for (const mountName of profileMounts) {
    const found = templates.get(mountName);
    if (!found) {
      skipped.push(`${mountName}: no providers/*/dev/mount.json template`);
      continue;
    }

    // The checked-in db/k8s templates use container-shaped paths
    // (`/data/test.db`, `unix:///run/omnifs/k8s.sock`), but the daemon is
    // host-native. Render absolute host-visible fixture paths under `devHome`
    // per session instead of baking checkout-specific paths into the template.
    if (mountName === "db") {
      const spec = structuredClone(found.template);
      spec.config = { ...spec.config, path: fixturePaths.dbPath };
      assertProviderInStore(index, spec.provider);
      mounts.push({ name: mountName, provider: spec.provider, template: spec });
      continue;
    }
    if (mountName === "k8s") {
      // Docker Desktop for macOS does not proxy a live AF_UNIX connection
      // through a bind mount: a socket file created inside a container shows
      // up on the host side of the bind as a regular (unconnectable) file, so
      // a host-native daemon on macOS cannot dial it. Linux bind mounts are
      // same-kernel, so the socket is real there. A TCP-published
      // `kubectl proxy` endpoint would work on both, but the kubernetes
      // provider's `endpoint` config is `HostSocket`-typed (unix-only);
      // widening it is a provider capability change. Named limitation, not a
      // silent drop.
      if (process.platform === "darwin") {
        skipped.push(
          `${mountName}: host-native daemon on macOS cannot reach a Docker bind-mounted unix ` +
            "socket (Docker Desktop does not proxy AF_UNIX connections across its VM boundary); " +
            "the provider accepts only a Unix socket endpoint",
        );
        continue;
      }
      const spec = structuredClone(found.template);
      spec.config = { ...spec.config, endpoint: `unix://${fixturePaths.k8sSockPath}` };
      assertProviderInStore(index, spec.provider);
      mounts.push({ name: mountName, provider: spec.provider, template: spec });
      continue;
    }

    const spec = structuredClone(found.template);
    const providerName = spec.provider;
    assertProviderInStore(index, providerName);

    const tokenEnv = DEV_TOKEN_ENV[providerName];
    if (tokenEnv) {
      const token = await resolveToken(providerName, tokenEnv, options);
      if (!token) {
        skipped.push(`${mountName}: missing ${tokenEnv}`);
        continue;
      }
      credentialEnv[tokenEnv] = token;
    }

    mounts.push({ name: mountName, provider: providerName, template: spec, tokenEnv });
  }

  return { mounts, skipped, credentialEnv };
}

function assertProviderInStore(index: ProviderStoreIndex, providerName: string): void {
  if (index.version !== 2) {
    throw new Error(`provider store bundle has unsupported index version ${index.version}`);
  }
  const entry = index.providers.find((candidate) => candidate.name === providerName);
  if (!entry) {
    throw new Error(`provider store bundle index has no exact entry for ${providerName}`);
  }
}

async function resolveToken(
  providerName: string,
  tokenEnv: string,
  options: DevOptions,
): Promise<string | null> {
  const fromEnv = process.env[tokenEnv];
  if (fromEnv) {
    return fromEnv;
  }

  if (providerName !== "github" || !commandExists("gh")) {
    return null;
  }

  if (!options.yes) {
    const allowed = await confirm("Use `gh auth token` for the GitHub dev credential?", true);
    if (!allowed) {
      return null;
    }
  }

  const token = (await awaitText($`gh auth token`)).trim();
  return token || null;
}

function printPlan({
  devHome,
  filesystemImage,
  profile,
  render,
  keepRunning,
}: {
  devHome: string;
  filesystemImage: string;
  profile: string;
  render: DevHomeRender;
  keepRunning: boolean;
}): void {
  console.log("");
  console.log("omnifs contributor dev session");
  console.log(`  Profile         ${profile}`);
  console.log(`  Mounts          ${render.mounts.map((mount) => mount.name).join(", ")}`);
  if (render.skipped.length > 0) {
    console.log(`  Skipped         ${render.skipped.join("; ")}`);
  }
  console.log(`  Filesystem image  ${filesystemImage}`);
  console.log(`  Dev home        ${devHome}`);
  console.log("");
  if (keepRunning) {
    console.log("Start the native daemon and the filesystem container, then return.");
  } else {
    console.log(`Start the native daemon and the filesystem container, then open a shell at ${GUEST_MOUNT}.`);
  }
  console.log("");
}

/// Env for every `omnifsCli` invocation against this session's dev home.
function cliEnv(devHome: string, extra: Record<string, string | undefined> = {}): Record<string, string | undefined> {
  return { ...process.env, ...extra, OMNIFS_HOME: devHome };
}

function writeDevConfig(
  devHome: string,
  providerStore: string,
  render: DevHomeRender,
  filesystemImage: string,
  hostProtocol: "fuse" | "nfs",
  hostLocation: string,
): string {
  mkdirSync(devHome, { recursive: true });
  chmodPrivateDir(devHome);
  const index = JSON.parse(
    readFileSync(join(providerStore, "index.json"), "utf8"),
  ) as ProviderStoreIndex;
  const resources: unknown[] = [];
  const providerNames = [...new Set(render.mounts.map((mount) => mount.provider))].sort();
  for (const providerName of providerNames) {
    const provider = index.providers.find((candidate) => candidate.name === providerName);
    if (!provider) {
      throw new Error(`provider store bundle index has no exact entry for ${providerName}`);
    }
    resources.push({
      kind: "Provider",
      spec: {
        name: providerName,
        source: {
          local: {
            path: join(providerStore, `${provider.id}.wasm`),
            expectedDigest: provider.id,
          },
        },
      },
    });
  }
  for (const mount of render.mounts) {
    let credential: string | undefined;
    const auth = mount.template.auth;
    if (auth) {
      if (auth.type !== "static-token" || !auth.scheme || !mount.tokenEnv) {
        throw new Error(`${mount.name}: unsupported dev auth template ${JSON.stringify(auth)}`);
      }
      credential = credentialName(mount);
      resources.push({
        kind: "Credential",
        spec: {
          name: credential,
          provider: mount.provider,
          scheme: auth.scheme,
          account: "default",
        },
      });
    }
    const limits = mount.template.limits as
      | { max_memory_mb?: number; max_fetch_blob_bytes?: number }
      | undefined;
    resources.push({
      kind: "Mount",
      spec: {
        name: mount.name,
        provider: mount.provider,
        ...(credential ? { credential } : {}),
        config: mount.template.config ?? {},
        ...(limits
          ? {
              limits: {
                ...(limits.max_memory_mb === undefined
                  ? {}
                  : { maxMemoryMb: limits.max_memory_mb }),
                ...(limits.max_fetch_blob_bytes === undefined
                  ? {}
                  : { maxFetchBlobBytes: limits.max_fetch_blob_bytes }),
              },
            }
          : {}),
      },
    });
  }
  resources.push(
    {
      kind: "Filesystem",
      spec: {
        name: "dev-host",
        protocol: hostProtocol,
        runtime: "host",
        location: hostLocation,
      },
    },
    {
      kind: "Filesystem",
      spec: {
        name: "dev-docker",
        protocol: "fuse",
        runtime: "docker",
        location: GUEST_MOUNT,
        dockerImage: filesystemImage,
      },
    },
  );
  const configPath = join(devHome, "dev.omnifs.k");
  const source =
    "# Generated by just dev. Secrets stay in environment variables.\n" +
    `config = ${renderKclValue({
      apiVersion: "omnifs.dev/v1alpha1",
      resources,
    })}\n`;
  writeFileSync(configPath, source, { mode: 0o600 });
  return configPath;
}

function credentialName(mount: DevMountRender): string {
  return `${mount.name}-credential`;
}

async function applyDevConfig(
  omnifsCli: string,
  configPath: string,
  env: Record<string, string | undefined>,
  credentialNames: string[],
): Promise<string> {
  const child = Bun.spawn(
    [omnifsCli, "apply", configPath, "--yes", "--output", "json"],
    {
      cwd: workspace,
      env,
      stdin: "ignore",
      stdout: "pipe",
      stderr: "inherit",
    },
  );
  const stdout = await new Response(child.stdout).text();
  const code = await child.exited;
  const envelope = JSON.parse(stdout) as {
    verdict: string;
    result?: { receipt?: { revision: number } };
    error?: {
      id: string;
      message: string;
      details?: { receipt?: { revision: number } };
    };
  };
  const revision =
    envelope.result?.receipt?.revision ??
    envelope.error?.details?.receipt?.revision;
  if (code === 0 && revision !== undefined) {
    return String(revision);
  }
  // A declaration may need credential material before the revision can
  // serve. Apply has still committed the exact desired set. Continue only
  // for that typed post-commit outcome, set secrets through durable actions,
  // then follow the same revision below.
  if (
    envelope.error?.id === "reconcile-failed" &&
    revision !== undefined &&
    credentialNames.some((name) => envelope.error?.message.includes(name))
  ) {
    return String(revision);
  }
  throw new Error(
    `declarative apply failed with status ${code}: ${stdout.trim()}`,
  );
}

function renderKclValue(value: unknown): string {
  if (value === null || value === undefined) {
    return "None";
  }
  if (typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    return String(value);
  }
  if (typeof value === "boolean") {
    return value ? "True" : "False";
  }
  if (Array.isArray(value)) {
    return `[${value.map(renderKclValue).join(", ")}]`;
  }
  const entries = Object.entries(value as Record<string, unknown>);
  return `{${entries
    .map(([key, item]) => `${JSON.stringify(key)} = ${renderKclValue(item)}`)
    .join(", ")}}`;
}

async function startFixtures(mounts: DevMountRender[], fixturePaths: FixturePaths): Promise<Fixtures> {
  const mountNames = new Set(mounts.map((mount) => mount.name));
  const fixtures: Fixtures = {
    k8s: false,
    dbContainerId: null,
  };

  if (mountNames.has("db")) {
    const dbDir = dirname(fixturePaths.dbPath);
    mkdirSync(dbDir, { recursive: true });
    await run($`docker build -t ${DB_IMAGE} .`.cwd(join(workspace, "providers/db/dev")));
    await removeContainer(DB_CONTAINER);
    fixtures.dbContainerId = (await awaitText(
      $`docker run -d --name ${DB_CONTAINER} -v ${`${dbDir}:/data`} ${DB_IMAGE}`,
    )).trim();
    await waitForFile(fixturePaths.dbPath, "db fixture seed");
  }

  if (mountNames.has("k8s")) {
    const sockDir = dirname(fixturePaths.k8sSockPath);
    mkdirSync(sockDir, { recursive: true });
    await run($`docker compose -p ${K8S_COMPOSE_PROJECT} -f ${join(
      workspace,
      "providers/kubernetes/dev/compose.yaml",
    )} up -d --wait`.env({ ...process.env, OMNIFS_K8S_SOCK_DIR: sockDir }));
    fixtures.k8s = true;
    await waitForFile(fixturePaths.k8sSockPath, "k8s proxy socket");
  }

  return fixtures;
}

async function teardownSession(
  devHome: string,
  omnifsCli: string,
  fixturePaths: FixturePaths,
  fixtures: Fixtures,
): Promise<void> {
  await $`${omnifsCli} down`.env(cliEnv(devHome)).quiet().nothrow();
  if (fixtures.k8s) {
    await run(
      $`docker compose -p ${K8S_COMPOSE_PROJECT} -f ${join(
        workspace,
        "providers/kubernetes/dev/compose.yaml",
      )} down -v`
        .env({ ...process.env, OMNIFS_K8S_SOCK_DIR: dirname(fixturePaths.k8sSockPath) })
        .nothrow(),
    );
  }
  if (fixtures.dbContainerId) {
    await removeContainer(fixtures.dbContainerId);
  }
}

async function removeContainer(name: string): Promise<void> {
  await run($`docker rm -f ${name}`.quiet().nothrow());
}

async function tagFloatingFilesystemImage(image: string): Promise<void> {
  if (image === FILESYSTEM_DEV_IMAGE) {
    return;
  }
  await run($`docker tag ${image} ${FILESYSTEM_DEV_IMAGE}`);
}

function buildFilesystemImage(image: string): Promise<void> {
  // No `provider-wasm` build context: the filesystem image runs the slim
  // `omnifs-thin --protocol fuse` binary (`thin-builder` stage), which needs no engine runtime, no
  // Wasmtime, and no provider bundle.
  return run($`docker build -t ${image} --target filesystem-dev .`);
}

async function gitShortHead(): Promise<string> {
  return (await awaitText($`git rev-parse --short=12 HEAD`)).trim();
}

function keepRunning(options: DevOptions): boolean {
  return options.detach || options.noShell;
}

function chmodPrivateDir(path: string): void {
  try {
    chmodSync(path, 0o700);
  } catch {
    // Best effort on non-Unix filesystems.
  }
}

function assertFile(path: string, label: string): void {
  if (!existsSync(path)) {
    throw new Error(`missing ${label} at ${path}`);
  }
}

/// Poll for `path` to appear (a fixture container seeding a file or a socket
/// coming up), bailing with a clear error instead of letting a later daemon
/// reconcile fail with a confusing "no such file" deep in its own log.
async function waitForFile(path: string, label: string, timeoutMs = 15000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!existsSync(path)) {
    if (Date.now() >= deadline) {
      throw new Error(`${label} did not appear at ${path} within ${timeoutMs}ms`);
    }
    await sleep(200);
  }
}

async function confirm(question: string, defaultYes: boolean): Promise<boolean> {
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    return false;
  }
  const suffix = defaultYes ? "[Y/n]" : "[y/N]";
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    const answer = (await rl.question(`${question} ${suffix} `)).trim().toLowerCase();
    if (!answer) {
      return defaultYes;
    }
    return answer === "y" || answer === "yes";
  } finally {
    rl.close();
  }
}

async function run(command: ShellCommand): Promise<void> {
  await command;
}

async function awaitText(command: ShellCommand): Promise<string> {
  return command.quiet().text();
}

async function runInteractive(
  args: string[],
  env: Record<string, string | undefined> = process.env,
): Promise<void> {
  const child = Bun.spawn(args, {
    cwd: workspace,
    env,
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
  });
  const code = await child.exited;
  if (code !== 0) {
    throw new Error(`${args[0]} exited with status ${code}`);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, ms));
}
