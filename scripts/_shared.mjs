#!/usr/bin/env bun

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

export const __dirname = path.dirname(fileURLToPath(import.meta.url));
export const rootDir = path.join(__dirname, '..');
export const cargoTomlPath = path.join(rootDir, 'Cargo.toml');
export const cargoLockPath = path.join(rootDir, 'Cargo.lock');
export const packageJsonPath = path.join(rootDir, 'package.json');

export function loadPackageJson() {
  return JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
}

export function writePackageJson(packageJson) {
  fs.writeFileSync(packageJsonPath, `${JSON.stringify(packageJson, null, 2)}\n`, 'utf8');
}

export function readCargoVersion() {
  const content = fs.readFileSync(cargoTomlPath, 'utf8');
  const match = content.match(/^version = "([^"]+)"/m);
  if (!match) {
    throw new Error('Could not find version in Cargo.toml');
  }
  return match[1];
}

export function writeCargoVersion(nextVersion) {
  const content = fs.readFileSync(cargoTomlPath, 'utf8');
  const updated = content.replace(/^version = "[^"]+"/m, `version = "${nextVersion}"`);
  fs.writeFileSync(cargoTomlPath, updated, 'utf8');
}

export function writeCargoLockVersion(nextVersion) {
  if (!fs.existsSync(cargoLockPath)) {
    return;
  }

  const packageName = loadPackageJson().name;
  const escapedPackageName = packageName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const pattern = new RegExp(
    `(\\[\\[package\\]\\]\\nname = "${escapedPackageName}"\\nversion = ")[^"]+(")`,
  );
  const content = fs.readFileSync(cargoLockPath, 'utf8');
  const updated = content.replace(pattern, `$1${nextVersion}$2`);

  if (updated === content) {
    throw new Error(`Could not find ${packageName} package entry in Cargo.lock`);
  }

  fs.writeFileSync(cargoLockPath, updated, 'utf8');
}

export function ensureVersionSync() {
  const packageVersion = loadPackageJson().version;
  const cargoVersion = readCargoVersion();
  if (packageVersion !== cargoVersion) {
    throw new Error(`Version mismatch: package.json=${packageVersion}, Cargo.toml=${cargoVersion}`);
  }
  return packageVersion;
}

function parseVersion(version) {
  const parts = version.split('.').map(Number);
  if (parts.length !== 3 || parts.some((part) => !Number.isInteger(part) || part < 0)) {
    throw new Error(`Unsupported version format: ${version}`);
  }
  return parts;
}

export function incrementPatch(version) {
  const [major, minor, patch] = parseVersion(version);
  return `${major}.${minor}.${patch + 1}`;
}

export function incrementMinor(version) {
  const [major, minor] = parseVersion(version);
  return `${major}.${minor + 1}.0`;
}

export function updateVersionEverywhere(nextVersion) {
  const packageJson = loadPackageJson();
  packageJson.version = nextVersion;
  writePackageJson(packageJson);
  writeCargoVersion(nextVersion);
  writeCargoLockVersion(nextVersion);
}
