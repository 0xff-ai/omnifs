#!/usr/bin/env bun

import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
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
  frontendImage: string | null;
  yes: boolean;
  detach: boolean;
  noShell: boolean;
  home: string | null;
  providerStore: string | null;
  skipCliBuild: boolean;
  buildOnly: boolean;
};

type ProviderStoreIndex = {
  latest?: Record<string, string>;
  providers: Array<{ id: string }>;
};

type DevMountTemplate = {
  mount: string;
  provider: string;
  auth?: {
    type?: string;
    scheme?: string;
  };
  config?: Record<string, unknown>;
  capabilities?: unknown;
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
const FRONTEND_DEV_IMAGE = "omnifs-frontend:dev";
// The daemon's own guest-mount constant (`crates/omnifs-cli/src/launch_backend.rs`
// `GUEST_MOUNT`); the frontend container always mounts here.
const GUEST_MOUNT = "/omnifs";
// The frontend image ships a minimal Debian base (fuse3, coreutils, findutils,
// jq, rsync, tar, xxd) with no bash/zsh (`Dockerfile`'s `frontend-base`), so
// the interactive dev shell and any container-side probe use POSIX `/bin/sh`.
const GUEST_SHELL = "/bin/sh";
// Label `crates/omnifs-cli/src/frontend_container.rs` stamps on the frontend
// container with the workspace's config dir (== `OMNIFS_HOME`): the single
// owner of the home->container-name mapping, so this script discovers the
// container by label instead of re-deriving its hashed name.
const FRONTEND_HOME_LABEL = "ai.0xff.omnifs.home";

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
  const frontendImage = options.frontendImage || `omnifs-frontend:${await gitShortHead()}-dev`;
  const providerStore = resolve(
    options.providerStore || join(workspace, "target/omnifs-provider-store"),
  );

  console.log(`Workspace: ${workspace}`);
  if (!options.providerStore) {
    await run($`just providers build`);
  }
  assertFile(join(providerStore, "index.json"), "provider store bundle");

  const builds: Promise<void>[] = [];
  if (!options.skipCliBuild) {
    // Default features (not `--no-default-features`): the `daemon` feature
    // pulls in `omnifs-daemon`/`omnifs-nfs`, without which `omnifs up` cannot
    // launch a host-native daemon at all (`launch_backend.rs`'s non-daemon
    // stub always bails).
    builds.push(
      run($`cargo build -p omnifs-cli`.env({
        ...process.env,
        OMNIFS_PROVIDER_BUNDLE_DIR: providerStore,
      })),
    );
  }
  if (!options.frontendImage) {
    builds.push(buildFrontendImage(frontendImage).then(() => tagFloatingFrontendImage(frontendImage)));
  }
  await Promise.all(builds);

  if (options.buildOnly) {
    const built = [];
    if (!options.skipCliBuild) {
      built.push("the omnifs CLI");
    }
    if (!options.frontendImage) {
      const tags = frontendImage === FRONTEND_DEV_IMAGE ? [frontendImage] : [frontendImage, FRONTEND_DEV_IMAGE];
      built.push(`the frontend image (${tags.join(" and ")})`);
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
      frontendImage,
      profile: options.profile,
      render,
      keepRunning: keepRunning(options),
    });
    const proceed = await confirm("Proceed?", true);
    if (!proceed) {
      throw new Error("aborted by user");
    }
  }

  await writeDevHome(devHome, providerStore, frontendImage, omnifsCli, render);

  const fixtures = await startFixtures(render.mounts, fixturePaths);
  try {
    console.log("Starting the host-native daemon");
    await run($`${omnifsCli} up --no-frontend`.env(cliEnv(devHome)));

    console.log("Starting the Docker-hosted FUSE frontend");
    await run($`${omnifsCli} frontend up --driver docker`.env(cliEnv(devHome)));

    const frontendContainer = await discoverFrontendContainer(devHome);
    if (keepRunning(options)) {
      if (options.detach) {
        console.log(`Detached. Stop with \`${omnifsCli} frontend down && ${omnifsCli} down\`.`);
      }
      return;
    }

    try {
      console.log(`Opening a shell in \`${frontendContainer}\` at ${GUEST_MOUNT}`);
      await runInteractive([
        "docker",
        "exec",
        "-it",
        "-w",
        GUEST_MOUNT,
        frontendContainer,
        GUEST_SHELL,
      ]);
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
    frontendImage: null,
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
    } else if (arg === "--frontend-image") {
      options.frontendImage = requireValue(args, ++i, "--frontend-image");
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
  if (!options.frontendImage) {
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

/// Resolve the `omnifs` binary this script drives: a fresh local build first,
/// else whatever `omnifs` is on `PATH` (the CI smoke lane installs a prebuilt
/// release CLI there via the `omnifs-install-cli` action and passes
/// `--skip-cli-build`).
function resolveCli(): string {
  const built = join(workspace, "target/debug/omnifs");
  if (existsSync(built)) {
    return built;
  }
  const onPath = Bun.which("omnifs");
  if (onPath) {
    return onPath;
  }
  throw new Error(
    "no omnifs CLI found: build one (drop --skip-cli-build) or put a prebuilt `omnifs` on PATH",
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
    // per session instead of baking workspace-specific paths into the template.
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
  const id = index.latest?.[providerName];
  if (!id) {
    throw new Error(`provider store bundle has no latest entry for ${providerName}`);
  }
  const entry = index.providers.find((candidate) => candidate.id === id);
  if (!entry) {
    throw new Error(`provider store bundle index is missing provider ${id}`);
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
  frontendImage,
  profile,
  render,
  keepRunning,
}: {
  devHome: string;
  frontendImage: string;
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
  console.log(`  Frontend image  ${frontendImage}`);
  console.log(`  Dev home        ${devHome}`);
  console.log("");
  if (keepRunning) {
    console.log("Start the native daemon and the frontend container, then return.");
  } else {
    console.log(`Start the native daemon and the frontend container, then open a shell at ${GUEST_MOUNT}.`);
  }
  console.log("");
}

/// Env for every `omnifsCli` invocation against this session's dev home.
function cliEnv(devHome: string, extra: Record<string, string | undefined> = {}): Record<string, string | undefined> {
  return { ...process.env, ...extra, OMNIFS_HOME: devHome };
}

async function writeDevHome(
  devHome: string,
  providerStore: string,
  frontendImage: string,
  omnifsCli: string,
  render: DevHomeRender,
): Promise<void> {
  mkdirSync(devHome, { recursive: true });
  chmodPrivateDir(devHome);

  const mountsDir = join(devHome, "mounts");
  const providersDir = join(devHome, "providers");
  const credentialsPath = join(devHome, "credentials.json");
  rmSync(mountsDir, { recursive: true, force: true });
  rmSync(providersDir, { recursive: true, force: true });
  rmSync(credentialsPath, { force: true });
  mkdirSync(mountsDir, { recursive: true });
  cpSync(providerStore, providersDir, { recursive: true });

  // The daemon always runs host-native; there is no runtime choice to
  // persist for this throwaway dev home. The only setting the native flow
  // needs is where to find the frontend image `omnifs frontend up` attaches.
  writeFileSync(
    join(devHome, "config.toml"),
    `[system]\nfrontend_image = ${JSON.stringify(frontendImage)}\n`,
  );

  for (const mount of render.mounts) {
    await runInitMount(devHome, omnifsCli, mount, render.credentialEnv);
  }
  if (existsSync(credentialsPath)) {
    chmodPrivateFile(credentialsPath);
  }
}

async function runInitMount(
  devHome: string,
  omnifsCli: string,
  mount: DevMountRender,
  credentialEnv: Record<string, string>,
): Promise<void> {
  const args = [
    "mount",
    "add",
    mount.provider,
    "--as",
    mount.name,
    "--no-input",
    "--yes",
  ];
  if (Object.prototype.hasOwnProperty.call(mount.template, "auth")) {
    const auth = mount.template.auth;
    if (auth?.type === "static-token") {
      if (auth.scheme) {
        args.push("--scheme", auth.scheme);
      }
      if (mount.tokenEnv) {
        // Dev/CI tokens (e.g. the Actions integration token) can fail the
        // provider's validation probe while working for their scope; dev
        // rendering stores them unvalidated, as it always has.
        args.push("--token-env", mount.tokenEnv, "--no-validate");
      }
    } else {
      throw new Error(`${mount.name}: unsupported dev auth template ${JSON.stringify(auth)}`);
    }
  } else {
    args.push("--no-auth");
  }
  if (mount.template.config) {
    args.push("--config-json", JSON.stringify(mount.template.config));
  }
  if (mount.template.capabilities) {
    args.push("--capabilities-json", JSON.stringify(mount.template.capabilities));
  }
  if (mount.template.limits) {
    args.push("--limits-json", JSON.stringify(mount.template.limits));
  }

  await run(
    $`${omnifsCli} ${args}`.env(
      // `init` never needs a live daemon; point at the discard port so an
      // accidental daemon touch fails fast instead of hanging.
      cliEnv(devHome, { ...credentialEnv, OMNIFS_DAEMON_ADDR: "127.0.0.1:9" }),
    ),
  );
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

/// Discover the running frontend container by the workspace-home label
/// `crates/omnifs-cli/src/frontend_container.rs` stamps on it, rather than
/// re-deriving its (possibly hashed) name in this script.
async function discoverFrontendContainer(devHome: string): Promise<string> {
  const name = (await awaitText(
    $`docker ps --filter ${`label=${FRONTEND_HOME_LABEL}=${devHome}`} --format {{.Names}}`.quiet(),
  )).trim();
  if (!name) {
    throw new Error(
      `no frontend container found for home ${devHome}; \`omnifs frontend up\` may have exited without one`,
    );
  }
  return name;
}

async function teardownSession(
  devHome: string,
  omnifsCli: string,
  fixturePaths: FixturePaths,
  fixtures: Fixtures,
): Promise<void> {
  await $`${omnifsCli} frontend down`.env(cliEnv(devHome)).quiet().nothrow();
  await $`${omnifsCli} down --force`.env(cliEnv(devHome)).quiet().nothrow();
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

async function tagFloatingFrontendImage(image: string): Promise<void> {
  if (image === FRONTEND_DEV_IMAGE) {
    return;
  }
  await run($`docker tag ${image} ${FRONTEND_DEV_IMAGE}`);
}

function buildFrontendImage(image: string): Promise<void> {
  // No `provider-wasm` build context: the frontend image runs the slim
  // `omnifs-fuse` binary (`fuse-builder` stage), which needs no engine, no
  // Wasmtime, and no provider bundle.
  return run($`docker build -t ${image} --target frontend-dev .`);
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

function chmodPrivateFile(path: string): void {
  try {
    chmodSync(path, 0o600);
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

async function runInteractive(args: string[]): Promise<void> {
  const child = Bun.spawn(args, {
    cwd: workspace,
    env: process.env,
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
