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
  image: string | null;
  yes: boolean;
  detach: boolean;
  noShell: boolean;
  home: string | null;
  providerStore: string | null;
  skipCliBuild: boolean;
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
  config?: unknown;
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

type Fixtures = {
  k8s: boolean;
  k8sSockDir: string | null;
  dbContainerId: string | null;
  binds: string[];
};

type ReconcileReport = {
  failed?: Array<{ mount: string; reason: string }>;
};

const $ = Bun.$;
const CONTAINER_NAME = process.env.OMNIFS_CONTAINER_NAME || "omnifs";
const DB_IMAGE = "omnifs-dev-db:local";
const DB_CONTAINER = "omnifs-dev-db";
const K8S_COMPOSE_PROJECT = "omnifs-devcluster";
const CONTROL_ADDR = "127.0.0.1:7878";
// The dev launcher's view of the container's guest paths (declared as image ENV
// in Dockerfile). Used for the home bind mount and the mount-readiness wait.
const GUEST_HOME = "/root/.omnifs";
const GUEST_MOUNT = "/omnifs";
const GUEST_SHELL = "/bin/zsh";

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
  const image = options.image || `omnifs:${await gitShortHead()}-dev`;
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
    builds.push(
      run($`cargo build -p omnifs-cli --no-default-features`.env({
        ...process.env,
        OMNIFS_PROVIDER_BUNDLE_DIR: providerStore,
      })),
    );
  }
  if (!options.image) {
    builds.push(buildImage(image, providerStore));
  }
  await Promise.all(builds);

  const render = await renderDevHomePlan(profileMounts, providerStore, options);
  if (render.mounts.length === 0) {
    throw new Error(`profile ${options.profile} rendered no usable mounts`);
  }

  if (!options.yes) {
    printPlan({
      devHome,
      image,
      profile: options.profile,
      render,
      keepRunning: keepRunning(options),
    });
    const proceed = await confirm("Proceed?", true);
    if (!proceed) {
      throw new Error("aborted by user");
    }
  }

  await writeDevHome(devHome, providerStore, image, render);

  const fixtures = await startFixtures(render.mounts, devHome);
  try {
    await launchContainer({ devHome, image, fixtures });
    await waitForReady();
    await reconcile();

    console.log(`✓ ${GUEST_MOUNT} is ready inside \`${CONTAINER_NAME}\``);
    if (keepRunning(options)) {
      if (options.detach) {
        console.log(`Detached. Stop with \`docker rm -f ${CONTAINER_NAME}\`.`);
      }
      return;
    }

    try {
      await runInteractive([
        "docker",
        "exec",
        "-it",
        "-w",
        GUEST_MOUNT,
        CONTAINER_NAME,
        GUEST_SHELL,
      ]);
    } finally {
      await teardownSession(fixtures);
    }
  } catch (error) {
    await teardownSession(fixtures);
    throw error;
  }
}

function parseArgs(args: string[]): DevOptions {
  const options: DevOptions = {
    profile: "default",
    image: null,
    yes: false,
    detach: false,
    noShell: false,
    home: null,
    providerStore: null,
    skipCliBuild: false,
  };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "-y" || arg === "--yes" || arg === "/y") {
      options.yes = true;
    } else if (arg === "--profile") {
      options.profile = requireValue(args, ++i, "--profile");
    } else if (arg === "--image") {
      options.image = requireValue(args, ++i, "--image");
    } else if (arg === "--home") {
      options.home = requireValue(args, ++i, "--home");
    } else if (arg === "--provider-store") {
      options.providerStore = requireValue(args, ++i, "--provider-store");
    } else if (arg === "--skip-cli-build") {
      options.skipCliBuild = true;
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
  if (!options.image) {
    commands.push("git");
  }

  for (const command of commands) {
    if (!(await commandExists(command))) {
      throw new Error(`missing prerequisite: ${command}`);
    }
  }
  if (!(await commandSucceeds($`docker info`.quiet().nothrow()))) {
    throw new Error("Docker daemon did not respond; start Docker and rerun");
  }
}

async function commandExists(command: string): Promise<boolean> {
  return commandSucceeds($`${command} --version`.quiet().nothrow());
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

async function renderDevHomePlan(
  profileMounts: string[],
  providerStore: string,
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

  if (providerName !== "github" || !(await commandExists("gh"))) {
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
  image,
  profile,
  render,
  keepRunning,
}: {
  devHome: string;
  image: string;
  profile: string;
  render: DevHomeRender;
  keepRunning: boolean;
}): void {
  console.log("");
  console.log("omnifs contributor dev session");
  console.log(`  Profile     ${profile}`);
  console.log(`  Mounts      ${render.mounts.map((mount) => mount.name).join(", ")}`);
  if (render.skipped.length > 0) {
    console.log(`  Skipped     ${render.skipped.join("; ")}`);
  }
  console.log(`  Image       ${image}`);
  console.log(`  Container   ${CONTAINER_NAME}`);
  console.log(`  Dev home    ${devHome}`);
  console.log("");
  if (keepRunning) {
    console.log("Bootstrap fixtures and runtime, then return.");
  } else {
    console.log(`Bootstrap fixtures and runtime, then open a shell at ${GUEST_MOUNT}.`);
  }
  console.log("");
}

async function writeDevHome(
  devHome: string,
  providerStore: string,
  image: string,
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

  writeFileSync(
    join(devHome, "config.toml"),
    `[system]\nruntime = "docker"\nimage = ${JSON.stringify(image)}\ncontainer_name = ${JSON.stringify(CONTAINER_NAME)}\n`,
  );

  for (const mount of render.mounts) {
    await runInitMount(devHome, image, mount, render.credentialEnv);
  }
  if (existsSync(credentialsPath)) {
    chmodPrivateFile(credentialsPath);
  }
}

async function runInitMount(
  devHome: string,
  image: string,
  mount: DevMountRender,
  credentialEnv: Record<string, string>,
): Promise<void> {
  const args = [
    "init",
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

  const hostCli = hostCliBinary();
  if (hostCli) {
    await run(
      $`${hostCli} ${args}`.env({
        ...process.env,
        ...credentialEnv,
        OMNIFS_HOME: devHome,
        OMNIFS_DAEMON_ADDR: "127.0.0.1:9",
      }),
    );
    return;
  }

  // No host-built CLI (the CI smoke lane passes --skip-cli-build and only has
  // the prebuilt image): render through the image's own binary in a one-shot
  // container against the same dev home. Token env vars pass by name only, so
  // their values never appear in the docker command line.
  const dockerArgs = [
    "run",
    "--rm",
    "--entrypoint",
    "/usr/local/bin/omnifs",
    "-v",
    `${devHome}:${GUEST_HOME}`,
    "-e",
    `OMNIFS_HOME=${GUEST_HOME}`,
    "-e",
    "OMNIFS_DAEMON_ADDR=127.0.0.1:9",
  ];
  if (mount.tokenEnv && credentialEnv[mount.tokenEnv] !== undefined) {
    dockerArgs.push("-e", mount.tokenEnv);
  }
  dockerArgs.push(image, ...args);
  await run(
    $`docker ${dockerArgs}`.env({
      ...process.env,
      ...credentialEnv,
    }),
  );
}

async function startFixtures(mounts: DevMountRender[], devHome: string): Promise<Fixtures> {
  const mountNames = new Set(mounts.map((mount) => mount.name));
  const fixtures: Fixtures = {
    k8s: false,
    k8sSockDir: null,
    dbContainerId: null,
    binds: [],
  };

  if (mountNames.has("db")) {
    const dbDir = join(devHome, "fixtures/db");
    mkdirSync(dbDir, { recursive: true });
    await run($`docker build -t ${DB_IMAGE} .`.cwd(join(workspace, "providers/db/dev")));
    await removeContainer(DB_CONTAINER);
    fixtures.dbContainerId = (await awaitText(
      $`docker run -d --name ${DB_CONTAINER} -v ${`${dbDir}:/data`} ${DB_IMAGE}`,
    )).trim();
    fixtures.binds.push(`${dbDir}:/data:ro`);
  }

  if (mountNames.has("k8s")) {
    const sockDir = join(devHome, "fixtures/k8s");
    mkdirSync(sockDir, { recursive: true });
    await run($`docker compose -p ${K8S_COMPOSE_PROJECT} -f ${join(
      workspace,
      "providers/kubernetes/dev/compose.yaml",
    )} up -d --wait`.env({ ...process.env, OMNIFS_K8S_SOCK_DIR: sockDir }));
    fixtures.k8s = true;
    fixtures.k8sSockDir = sockDir;
    fixtures.binds.push(`${sockDir}:/run/omnifs`);
  }

  return fixtures;
}

async function launchContainer({
  devHome,
  image,
  fixtures,
}: {
  devHome: string;
  image: string;
  fixtures: Fixtures;
}): Promise<void> {
  await removeContainer(CONTAINER_NAME);

  const args = [
    "run",
    "-d",
    "--name",
    CONTAINER_NAME,
    "-p",
    `${CONTROL_ADDR}:7878`,
    "-v",
    `${devHome}:${GUEST_HOME}`,
    "--device",
    "/dev/fuse",
    "--cap-add",
    "SYS_ADMIN",
    "--security-opt",
    "apparmor:unconfined",
    "-e",
    `OMNIFS_HOME=${GUEST_HOME}`,
    "-e",
    `OMNIFS_CONTAINER_NAME=${CONTAINER_NAME}`,
    "-e",
    `OMNIFS_IMAGE=${image}`,
    "-e",
    "SSH_AUTH_SOCK=/ssh-agent",
    "-e",
    "GIT_SSH_COMMAND=ssh -F /dev/null -o StrictHostKeyChecking=accept-new",
  ];

  if (process.env.SSH_AUTH_SOCK && existsSync(process.env.SSH_AUTH_SOCK)) {
    args.push("-v", `${process.env.SSH_AUTH_SOCK}:/ssh-agent`);
  }
  for (const bind of fixtures.binds) {
    args.push("-v", bind);
  }
  args.push(image);

  await run($`docker ${args}`);
}

async function waitForReady() {
  console.log(`Waiting for ${GUEST_MOUNT} inside \`${CONTAINER_NAME}\``);
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(`http://${CONTROL_ADDR}/v1/ready`);
      if (response.ok) {
        console.log("✓ FUSE mount is ready");
        return;
      }
    } catch {
      // Keep polling; Docker may not have published the port yet.
    }

    const state = (await awaitText(
      $`docker inspect -f "{{.State.Running}} {{.State.Status}} {{.State.ExitCode}}" ${CONTAINER_NAME}`
        .quiet()
        .nothrow(),
    )).trim();
    if (state && !state.startsWith("true ")) {
      throw new Error(
        `container \`${CONTAINER_NAME}\` exited before ${GUEST_MOUNT} became available (${state}); run \`docker logs ${CONTAINER_NAME}\``,
      );
    }
    await sleep(1000);
  }
  throw new Error(`${GUEST_MOUNT} did not become available inside \`${CONTAINER_NAME}\` within 60s`);
}

async function reconcile() {
  const token = await waitForControlToken();
  const report = await fetchJson<ReconcileReport>("/v1/reconcile", {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
  });
  for (const failure of report.failed || []) {
    console.error(`warning: mount \`${failure.mount}\` did not load: ${failure.reason}`);
  }
}

async function waitForControlToken(): Promise<string> {
  // The daemon writes the token file 0600 inside the container, so the host
  // user cannot read it through the bind mount; read it through the container.
  const deadline = Date.now() + 15_000;
  for (;;) {
    const token = (
      await awaitText($`docker exec ${CONTAINER_NAME} cat ${GUEST_HOME}/control-token`.quiet().nothrow())
    ).trim();
    if (token) {
      return token;
    }
    if (Date.now() >= deadline) {
      throw new Error(
        `control token not readable from ${CONTAINER_NAME}:${GUEST_HOME}/control-token after 15s; check \`docker logs ${CONTAINER_NAME}\``,
      );
    }
    await sleep(250);
  }
}

async function fetchJson<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(`http://${CONTROL_ADDR}${path}`, init);
  if (!response.ok) {
    throw new Error(`${init.method || "GET"} ${path} failed with HTTP ${response.status}`);
  }
  return response.json() as Promise<T>;
}

async function teardownSession(fixtures: Fixtures): Promise<void> {
  await removeContainer(CONTAINER_NAME);
  if (fixtures.k8s && fixtures.k8sSockDir) {
    await run(
      $`docker compose -p ${K8S_COMPOSE_PROJECT} -f ${join(
        workspace,
        "providers/kubernetes/dev/compose.yaml",
      )} down -v`
        .env({ ...process.env, OMNIFS_K8S_SOCK_DIR: fixtures.k8sSockDir })
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

function buildImage(image: string, providerStore: string): Promise<void> {
  return run(
    $`docker build -t ${image} --target runtime-dev --build-context ${`provider-wasm=${providerStore}`} --build-arg ${`OMNIFS_MIN_LAUNCHER_VERSION=${workspaceVersion()}`} .`,
  );
}

function workspaceVersion() {
  const raw = readFileSync(join(workspace, "Cargo.toml"), "utf8");
  const match = raw.match(/\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/m);
  return match?.[1] || "unknown";
}

async function gitShortHead(): Promise<string> {
  return (await awaitText($`git rev-parse --short=12 HEAD`)).trim();
}

function keepRunning(options: DevOptions): boolean {
  return options.detach || options.noShell;
}

function hostCliBinary(): string | null {
  const path = join(workspace, "target/debug/omnifs");
  return existsSync(path) ? path : null;
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

async function commandSucceeds(command: ShellCommand): Promise<boolean> {
  const output = await command;
  return output.exitCode === 0;
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
