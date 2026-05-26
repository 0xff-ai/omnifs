import { Glob } from "bun";
import { dirname, join } from "node:path";
import { errorMessage, Repo, printErrorsAndExit } from "./repo";

const ROOT_PACKAGE = "@0xff-ai/omnifs";

export type PlatformSpec = {
  package: string;
  rustTarget: string;
  os: string;
  cpu: string;
  runner: string;
};

export type PlatformCatalog = Record<string, PlatformSpec>;

type PackageJson = {
  path: string;
  name: string;
  version: string;
  os?: string[];
  cpu?: string[];
  optionalDependencies: Record<string, string>;
};

type NpmLayout = {
  rootPackage: PackageJson;
  platformPackages: Map<string, PackageJson>;
};

export class NpmWorkspace {
  constructor(private readonly repo: Repo) {}

  async loadCatalog(): Promise<PlatformCatalog> {
    return await this.repo.readJson<PlatformCatalog>("npm", "platforms.json");
  }

  async sync(version?: string): Promise<void> {
    const targetVersion = version ?? await this.repo.workspaceVersion();
    const catalog = await this.loadCatalog();
    const layout = await this.discoverLayout(catalog);
    await this.writeVersion(layout.rootPackage, targetVersion);
    for (const pkg of layout.platformPackages.values()) {
      await this.writeVersion(pkg, targetVersion);
    }
    console.log(`synced npm packages to version ${targetVersion}`);
  }

  async validateSynced(version?: string): Promise<string[]> {
    const targetVersion = version ?? await this.repo.workspaceVersion();
    const catalog = await this.loadCatalog();
    const layout = await this.discoverLayout(catalog);
    const errors: string[] = [];
    for (const pkg of allPackages(layout)) {
      if (pkg.version !== targetVersion) {
        errors.push(`${pkg.path} version ${pkg.version} != workspace version ${targetVersion}`);
      }
      if (pkg.name === ROOT_PACKAGE) {
        for (const [dep, depVersion] of Object.entries(pkg.optionalDependencies)) {
          if (depVersion !== targetVersion) {
            errors.push(`${pkg.path} optionalDependencies.${dep} version ${depVersion} != workspace version ${targetVersion}`);
          }
        }
      }
    }
    return errors;
  }

  async validate(): Promise<number> {
    const version = await this.repo.workspaceVersion();
    const errors: string[] = [];
    let catalog: PlatformCatalog;
    let layout: NpmLayout;

    try {
      catalog = await this.loadCatalog();
      layout = await this.discoverLayout(catalog);
    } catch (error) {
      errors.push(errorMessage(error));
      printErrorsAndExit("npm platform validation", errors);
    }

    validatePackages(version, catalog, layout, errors);
    validateRootOptionalDependencies(version, layout, catalog, errors);
    await this.validateDistTargets(catalog, errors);

    if (errors.length > 0) {
      printErrorsAndExit("npm platform validation", errors);
    }
    return Object.keys(catalog).length;
  }

  private async packageJson(path: string): Promise<PackageJson> {
    const pkg = await Bun.file(path).json() as Omit<PackageJson, "path" | "optionalDependencies"> & {
      optionalDependencies?: Record<string, string>;
    };
    return {
      path,
      name: pkg.name,
      version: pkg.version,
      os: pkg.os,
      cpu: pkg.cpu,
      optionalDependencies: pkg.optionalDependencies ?? {},
    };
  }

  private async discoverLayout(catalog: PlatformCatalog): Promise<NpmLayout> {
    const rootPackage = await this.packageJson(this.repo.path("npm", "omnifs", "package.json"));
    const platformDir = this.repo.path("npm", "platform");
    const platformPackages = new Map<string, PackageJson>();

    for (const platform of Object.keys(catalog)) {
      const path = join(platformDir, platform, "package.json");
      if (!await Bun.file(path).exists()) {
        throw new Error(`missing npm platform package at ${path}`);
      }
      platformPackages.set(platform, await this.packageJson(path));
    }

    for (const entry of platformDirEntries(platformDir)) {
      if (!entry.includes("/") && !entry.includes("\\") && !(entry in catalog)) {
        throw new Error(`npm/platform/${entry} is not declared in npm/platforms.json`);
      }
    }

    return { rootPackage, platformPackages };
  }

  private async writeVersion(pkg: PackageJson, version: string): Promise<void> {
    const args = ["pkg", "set", `version=${version}`];
    if (pkg.name === ROOT_PACKAGE) {
      for (const dep of Object.keys(pkg.optionalDependencies)) {
        args.push(`optionalDependencies.${dep}=${version}`);
      }
    }
    await this.repo.$`npm ${args} --prefix ${dirname(pkg.path)}`;
  }

  private async validateDistTargets(catalog: PlatformCatalog, errors: string[]): Promise<void> {
    let distWorkspace: { dist?: { targets?: string[] } };
    try {
      distWorkspace = await this.repo.readToml("dist-workspace.toml");
    } catch (error) {
      errors.push(errorMessage(error));
      return;
    }

    const actual = [...(distWorkspace.dist?.targets ?? [])].sort();
    const expected = Object.values(catalog)
      .filter((spec) => spec.os === "darwin")
      .map((spec) => spec.rustTarget)
      .sort();
    assertSetEqual(actual, expected, "dist-workspace.toml targets (macOS only; Linux CLI is built by native CI)", errors);
  }
}

function platformDirEntries(platformDir: string): string[] {
  try {
    return [...new Glob("*/").scanSync({ cwd: platformDir, onlyFiles: false })];
  } catch (error) {
    if (errorMessage(error).includes("ENOENT")) return [];
    throw error;
  }
}

function allPackages(layout: NpmLayout): PackageJson[] {
  return [...layout.platformPackages.values(), layout.rootPackage];
}

function validatePackages(version: string, catalog: PlatformCatalog, layout: NpmLayout, errors: string[]): void {
  for (const [platform, spec] of Object.entries(catalog)) {
    const pkg = layout.platformPackages.get(platform);
    if (!pkg) {
      errors.push(`missing npm platform package directory for ${platform}`);
      continue;
    }
    if (pkg.name !== spec.package) {
      errors.push(`${pkg.path} name ${pkg.name} != platforms.json package ${spec.package}`);
    }
    if (!arrayEquals(pkg.os, [spec.os])) {
      errors.push(`${pkg.path} os mismatch for ${platform}`);
    }
    if (!arrayEquals(pkg.cpu, [spec.cpu])) {
      errors.push(`${pkg.path} cpu mismatch for ${platform}`);
    }
    if (pkg.version !== version) {
      errors.push(`${pkg.path} version ${pkg.version} != Cargo.toml workspace version ${version}`);
    }
  }

  if (layout.rootPackage.version !== version) {
    errors.push(`${layout.rootPackage.path} version ${layout.rootPackage.version} != Cargo.toml workspace version ${version}`);
  }
}

function validateRootOptionalDependencies(
  version: string,
  layout: NpmLayout,
  catalog: PlatformCatalog,
  errors: string[],
): void {
  const actual = Object.keys(layout.rootPackage.optionalDependencies).sort();
  const expected = Object.values(catalog).map((spec) => spec.package).sort();
  assertSetEqual(actual, expected, "npm/omnifs/package.json optionalDependencies", errors);
  for (const [dep, depVersion] of Object.entries(layout.rootPackage.optionalDependencies)) {
    if (depVersion !== version) {
      errors.push(`${layout.rootPackage.path} optionalDependencies.${dep} version ${depVersion} != Cargo.toml workspace version ${version}`);
    }
  }
}

function assertSetEqual(actual: string[], expected: string[], label: string, errors: string[]): void {
  const actualSet = new Set(actual);
  const expectedSet = new Set(expected);
  for (const value of actualSet) {
    if (!expectedSet.has(value)) errors.push(`${label} has extra entry ${value}`);
  }
  for (const value of expectedSet) {
    if (!actualSet.has(value)) errors.push(`${label} missing entry ${value}`);
  }
}

function arrayEquals(left: string[] | undefined, right: string[]): boolean {
  if (!left || left.length !== right.length) return false;
  return left.every((value, index) => value === right[index]);
}
