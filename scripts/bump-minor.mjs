#!/usr/bin/env bun

import { ensureVersionSync, incrementMinor, updateVersionEverywhere } from './_shared.mjs';

const currentVersion = ensureVersionSync();
const nextVersion = incrementMinor(currentVersion);

updateVersionEverywhere(nextVersion);
console.log(`Bumped minor version: ${currentVersion} -> ${nextVersion}`);
