import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const FRONTEND_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const REPO_ROOT = path.resolve(FRONTEND_DIR, "..");
const GENERATED_ROOT = path.join(REPO_ROOT, "generated");
const ENV_PATH = path.join(FRONTEND_DIR, ".env.local");
const START_MARKER = "# BEGIN omega-zone generated config";
const END_MARKER = "# END omega-zone generated config";
const MANAGED_KEYS = new Set([
  "NEXT_PUBLIC_TEMPO_RPC",
  "NEXT_PUBLIC_TEMPO_CHAIN_ID",
  "NEXT_PUBLIC_ZONE_RPC",
  "NEXT_PUBLIC_ZONE_CHAIN_ID",
  "NEXT_PUBLIC_ZONE_ID",
  "NEXT_PUBLIC_ZONE_PORTAL",
]);

function stripManagedBlock(content) {
  if (!content.includes(START_MARKER) || !content.includes(END_MARKER)) {
    return stripManagedKeys(content);
  }

  const pattern = new RegExp(`${START_MARKER}[\\s\\S]*?${END_MARKER}\\n?`, "g");
  return stripManagedKeys(content.replace(pattern, ""));
}

function stripManagedKeys(content) {
  return content
    .split("\n")
    .filter((line) => {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith("#")) {
        return true;
      }

      const [key] = trimmed.split("=", 1);
      return !MANAGED_KEYS.has(key);
    })
    .join("\n")
    .trimEnd();
}

function resolveZoneDir() {
  const configuredName = process.env.OMEGA_ZONE_NAME?.trim();
  if (configuredName) {
    const zoneDir = path.join(GENERATED_ROOT, configuredName);
    if (!fs.existsSync(path.join(zoneDir, "zone.json"))) {
      throw new Error(
        `OMEGA_ZONE_NAME=${configuredName} does not contain generated zone metadata at ${path.relative(REPO_ROOT, path.join(zoneDir, "zone.json"))}`
      );
    }
    return zoneDir;
  }

  if (!fs.existsSync(GENERATED_ROOT)) {
    throw new Error("generated/ does not exist yet. Create a zone first.");
  }

  const candidates = fs
    .readdirSync(GENERATED_ROOT, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => path.join(GENERATED_ROOT, entry.name))
    .filter((zoneDir) => fs.existsSync(path.join(zoneDir, "zone.json")));

  if (candidates.length === 0) {
    throw new Error("No generated zone metadata found under generated/<name>/zone.json.");
  }

  if (candidates.length > 1) {
    const names = candidates.map((zoneDir) => path.basename(zoneDir)).join(", ");
    throw new Error(
      `Multiple generated zones found (${names}). Set OMEGA_ZONE_NAME to choose one.`
    );
  }

  return candidates[0];
}

function readZoneMetadata(zoneDir) {
  const zoneJsonPath = path.join(zoneDir, "zone.json");
  return JSON.parse(fs.readFileSync(zoneJsonPath, "utf8"));
}

function buildManagedBlock(zoneDir, metadata) {
  const zoneName = path.basename(zoneDir);
  const zoneRpc = process.env.NEXT_PUBLIC_ZONE_RPC || "http://localhost:8546";
  const tempoRpc =
    process.env.NEXT_PUBLIC_TEMPO_RPC || "https://rpc.moderato.tempo.xyz";

  return [
    START_MARKER,
    `# Auto-generated from generated/${zoneName}/zone.json`,
    "# Manual edits inside this block will be replaced.",
    `NEXT_PUBLIC_TEMPO_RPC=${tempoRpc}`,
    "NEXT_PUBLIC_TEMPO_CHAIN_ID=42431",
    `NEXT_PUBLIC_ZONE_RPC=${zoneRpc}`,
    `NEXT_PUBLIC_ZONE_CHAIN_ID=${metadata.chainId}`,
    `NEXT_PUBLIC_ZONE_ID=${metadata.zoneId}`,
    `NEXT_PUBLIC_ZONE_PORTAL=${metadata.portal}`,
    END_MARKER,
  ].join("\n");
}

function main() {
  const zoneDir = resolveZoneDir();
  const metadata = readZoneMetadata(zoneDir);

  if (
    metadata == null ||
    typeof metadata.chainId !== "number" ||
    typeof metadata.zoneId !== "number" ||
    typeof metadata.portal !== "string"
  ) {
    throw new Error(
      `Invalid zone metadata in ${path.relative(REPO_ROOT, path.join(zoneDir, "zone.json"))}. Expected chainId, zoneId, and portal.`
    );
  }

  const existing = fs.existsSync(ENV_PATH) ? fs.readFileSync(ENV_PATH, "utf8") : "";
  const unmanaged = stripManagedBlock(existing);
  const managedBlock = buildManagedBlock(zoneDir, metadata);
  const nextContent = `${managedBlock}\n${unmanaged ? `\n\n${unmanaged}\n` : "\n"}`;

  fs.writeFileSync(ENV_PATH, nextContent);
  console.log(
    `Synced frontend env from generated/${path.basename(zoneDir)}/zone.json -> frontend/.env.local`
  );
}

try {
  main();
} catch (error) {
  console.error(
    error instanceof Error ? `sync-zone-env: ${error.message}` : "sync-zone-env: unknown error"
  );
  process.exit(1);
}
