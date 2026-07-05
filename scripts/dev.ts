#!/usr/bin/env bun

import { $ } from "bun";
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
// rather than in the mount templates. The same env var name appears as a
// `${VAR}` placeholder in `contrib/dev-credentials.json`.
const DEV_TOKEN_ENV = { github: "GITHUB_TOKEN", linear: "LINEAR_API_KEY" };

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

  const builds = [];
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

  writeDevHome(devHome, providerStore, image, render);

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

function parseArgs(args) {
  const options = {
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

function requireValue(args, index, flag) {
  const value = args[index];
  if (!value || value.startsWith("--")) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}

async function checkPrerequisites(options) {
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

async function commandExists(command) {
  return commandSucceeds($`${command} --version`.quiet().nothrow());
}

function readProfile(profile) {
  const path = join(workspace, "contrib/dev-profiles", `${profile}.toml`);
  const raw = readFileSync(path, "utf8");
  const match = raw.match(/mounts\s*=\s*\[([^\]]*)\]/m);
  if (!match) {
    throw new Error(`profile ${path} does not define mounts = [...]`);
  }
  return [...match[1].matchAll(/"([^"]+)"/g)].map((item) => item[1]);
}

function discoverTemplates() {
  const providersDir = join(workspace, "providers");
  const templates = new Map();
  for (const provider of readdirSync(providersDir)) {
    const path = join(providersDir, provider, "dev/mount.json");
    if (!existsSync(path)) {
      continue;
    }
    const template = JSON.parse(readFileSync(path, "utf8"));
    templates.set(template.mount, { path, template });
  }
  return templates;
}

async function renderDevHomePlan(profileMounts, providerStore, options) {
  const index = JSON.parse(readFileSync(join(providerStore, "index.json"), "utf8"));
  const templates = discoverTemplates();
  const mounts = [];
  const skipped = [];
  const credentialEnv = {};

  for (const mountName of profileMounts) {
    const found = templates.get(mountName);
    if (!found) {
      skipped.push(`${mountName}: no providers/*/dev/mount.json template`);
      continue;
    }

    const spec = structuredClone(found.template);
    const providerName = spec.provider;
    spec.provider = providerRef(index, providerName);

    const tokenEnv = DEV_TOKEN_ENV[providerName];
    if (tokenEnv) {
      const token = await resolveToken(providerName, tokenEnv, options);
      if (!token) {
        skipped.push(`${mountName}: missing ${tokenEnv}`);
        continue;
      }
      credentialEnv[tokenEnv] = token;
    }

    mounts.push({ name: mountName, provider: providerName, spec });
  }

  return { mounts, skipped, credentialEnv };
}

function providerRef(index, providerName) {
  const id = index.latest?.[providerName];
  if (!id) {
    throw new Error(`provider store bundle has no latest entry for ${providerName}`);
  }
  const entry = index.providers.find((candidate) => candidate.id === id);
  if (!entry) {
    throw new Error(`provider store bundle index is missing provider ${id}`);
  }
  const meta = { name: entry.name };
  if (entry.version) {
    meta.version = entry.version;
  }
  return { id, meta };
}

async function resolveToken(providerName, tokenEnv, options) {
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

// Render the dev credential store from the checked-in template, replacing each
// `"${VAR}"` placeholder with the JSON-encoded env value dev resolved (so the
// token is escaped correctly), or `null` when the var is absent. The template
// (`contrib/dev-credentials.json`) owns the entry shape, mirroring
// `omnifs_creds::CredentialEntry`; dev only fills in secrets. Entries left
// without a token are dropped, so a mount with no token has no credential.
function renderCredentials(credentialEnv) {
  const raw = readFileSync(join(workspace, "contrib/dev-credentials.json"), "utf8");
  const filled = raw.replace(/"\$\{(\w+)\}"/g, (_, name) =>
    credentialEnv[name] === undefined ? "null" : JSON.stringify(credentialEnv[name]),
  );
  const store = JSON.parse(filled);
  store.entries = Object.fromEntries(
    Object.entries(store.entries).filter(([, entry]) => entry.access_token !== null),
  );
  return store;
}

function printPlan({ devHome, image, profile, render, keepRunning }) {
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

function writeDevHome(devHome, providerStore, image, render) {
  mkdirSync(devHome, { recursive: true });
  chmodPrivateDir(devHome);

  const mountsDir = join(devHome, "mounts");
  const providersDir = join(devHome, "providers");
  rmSync(mountsDir, { recursive: true, force: true });
  rmSync(providersDir, { recursive: true, force: true });
  mkdirSync(mountsDir, { recursive: true });
  cpSync(providerStore, providersDir, { recursive: true });

  writeFileSync(
    join(devHome, "config.toml"),
    `[system]\nruntime = "docker"\nimage = ${JSON.stringify(image)}\ncontainer_name = ${JSON.stringify(CONTAINER_NAME)}\n`,
  );

  for (const mount of render.mounts) {
    writeJson(join(mountsDir, `${mount.name}.json`), mount.spec);
  }

  const credentialsPath = join(devHome, "credentials.json");
  writeJson(credentialsPath, renderCredentials(render.credentialEnv));
  chmodPrivateFile(credentialsPath);
}

async function startFixtures(mounts, devHome) {
  const mountNames = new Set(mounts.map((mount) => mount.name));
  const fixtures = {
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

async function launchContainer({ devHome, image, fixtures }) {
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
  const report = await fetchJson("/v1/reconcile", { method: "POST" });
  for (const failure of report.failed || []) {
    console.error(`warning: mount \`${failure.mount}\` did not load: ${failure.reason}`);
  }
}

async function fetchJson(path, init = {}) {
  const response = await fetch(`http://${CONTROL_ADDR}${path}`, init);
  if (!response.ok) {
    throw new Error(`${init.method || "GET"} ${path} failed with HTTP ${response.status}`);
  }
  return response.json();
}

async function teardownSession(fixtures) {
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

async function removeContainer(name) {
  await run($`docker rm -f ${name}`.quiet().nothrow());
}

function buildImage(image, providerStore) {
  return run(
    $`docker build -t ${image} --target runtime-dev --build-context ${`provider-wasm=${providerStore}`} --build-arg ${`OMNIFS_MIN_LAUNCHER_VERSION=${workspaceVersion()}`} .`,
  );
}

function workspaceVersion() {
  const raw = readFileSync(join(workspace, "Cargo.toml"), "utf8");
  const match = raw.match(/\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/m);
  return match?.[1] || "unknown";
}

async function gitShortHead() {
  return (await awaitText($`git rev-parse --short=12 HEAD`)).trim();
}

function keepRunning(options) {
  return options.detach || options.noShell;
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

function chmodPrivateDir(path) {
  try {
    chmodSync(path, 0o700);
  } catch {
    // Best effort on non-Unix filesystems.
  }
}

function chmodPrivateFile(path) {
  try {
    chmodSync(path, 0o600);
  } catch {
    // Best effort on non-Unix filesystems.
  }
}

function assertFile(path, label) {
  if (!existsSync(path)) {
    throw new Error(`missing ${label} at ${path}`);
  }
}

async function confirm(question, defaultYes) {
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

async function run(command) {
  await command;
}

async function awaitText(command) {
  return command.quiet().text();
}

async function commandSucceeds(command) {
  const output = await command;
  return output.exitCode === 0;
}

async function runInteractive(args) {
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

function sleep(ms) {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, ms));
}
