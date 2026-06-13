export type Version = {
  major: number;
  minor: number;
  patch: number;
  prerelease?: string;
};

export function parseVersion(input: string): Version {
  const [core, prerelease] = input.split("-", 2);
  const parts = core.split(".");
  if (parts.length !== 3) {
    throw new Error(`invalid version: ${input}`);
  }
  const [major, minor, patch] = parts.map((part) => Number(part));
  if (![major, minor, patch].every(Number.isSafeInteger)) {
    throw new Error(`invalid version: ${input}`);
  }
  return { major, minor, patch, prerelease };
}

export function formatVersion(version: Version): string {
  const core = `${version.major}.${version.minor}.${version.patch}`;
  return version.prerelease ? `${core}-${version.prerelease}` : core;
}

export function bumpPatch(version: Version): Version {
  return {
    major: version.major,
    minor: version.minor,
    patch: version.patch + 1,
  };
}

export type BumpLevel = "major" | "minor" | "patch";

export function bump(version: Version, level: BumpLevel): Version {
  switch (level) {
    case "major":
      return { major: version.major + 1, minor: 0, patch: 0 };
    case "minor":
      return { major: version.major, minor: version.minor + 1, patch: 0 };
    case "patch":
      return bumpPatch(version);
  }
}
