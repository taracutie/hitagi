#!/usr/bin/env bun

import { ensureVersionSync, incrementPatch, updateVersionEverywhere } from './_shared.mjs';

const currentVersion = ensureVersionSync();
const nextVersion = incrementPatch(currentVersion);

updateVersionEverywhere(nextVersion);
console.log(`Bumped patch version: ${currentVersion} -> ${nextVersion}`);
