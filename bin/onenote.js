#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const SUPPORTED = [
  'win32-x64',
  'win32-arm64',
  'darwin-x64',
  'darwin-arm64',
  'linux-x64',
  'linux-arm64',
];

function fail(message) {
  process.stderr.write(`onenote: ${message}\n`);
  process.exit(1);
}

const platform = process.platform;
const arch = process.arch;
const platformKey = `${platform}-${arch}`;
const packageName = `@bitefoundry/onenote-${platformKey}`;
const binaryName = platform === 'win32' ? 'onenote.exe' : 'onenote';

let packageDir = null;
try {
  packageDir = path.dirname(require.resolve(`${packageName}/package.json`));
} catch {
  packageDir = null;
}

const binaryPath = packageDir ? path.join(packageDir, 'bin', binaryName) : null;

if (!binaryPath || !fs.existsSync(binaryPath)) {
  const lines = [
    `no prebuilt binary found for ${platformKey}.`,
    '',
    `Expected the optional dependency "${packageName}" to provide:`,
    `  ${binaryPath || `node_modules/${packageName}/bin/${binaryName}`}`,
    '',
    `Supported platforms: ${SUPPORTED.join(', ')}`,
    '',
    'Try one of:',
    '  npm install -g @bitefoundry/onenote --include=optional',
    '  npm install @bitefoundry/onenote --include=optional',
    '  npx --yes @bitefoundry/onenote',
    '',
    'If you are on an unsupported platform (for example Alpine/musl Linux),',
    'build from source with Cargo:',
    '  cargo build --release',
  ];
  fail(lines.join('\n'));
}

if (platform !== 'win32') {
  try {
    fs.chmodSync(binaryPath, 0o755);
  } catch {
    // Read-only install; spawn will report EACCES if the bit is missing.
  }
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: false,
});

child.on('error', (err) => {
  fail(`failed to launch ${binaryPath}: ${err.message}`);
});

for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  const handler = () => {
    if (!child.killed) {
      try {
        child.kill(signal);
      } catch {
        // Child already exited.
      }
    }
  };
  try {
    process.on(signal, handler);
  } catch {
    // Signal not supported on this platform.
  }
}

child.on('exit', (code, signal) => {
  if (signal) {
    const number = os.constants.signals[signal] || 1;
    process.exit(128 + number);
  }
  process.exit(code === null ? 1 : code);
});
