import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { runPostinstall } from '../scripts/postinstall.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const POSTINSTALL_SCRIPT = path.join(REPO_ROOT, 'scripts', 'postinstall.mjs');
const BIN_LAUNCHER = path.join(REPO_ROOT, 'bin', 'onenote.js');

function createFakeInstalledPkg(tmpRoot, { platform = process.platform, arch = process.arch } = {}) {
  const pkgRoot = path.join(tmpRoot, 'node_modules', '@bitefoundry', 'onenote');
  const binDir = path.join(pkgRoot, 'bin');
  const scriptsDir = path.join(pkgRoot, 'scripts');
  const platPkgName = `onenote-${platform}-${arch}`;
  const platPkgDir = path.join(pkgRoot, 'node_modules', '@bitefoundry', platPkgName);
  const platBinDir = path.join(platPkgDir, 'bin');
  const binaryName = platform === 'win32' ? 'onenote.exe' : 'onenote';

  fs.mkdirSync(binDir, { recursive: true });
  fs.mkdirSync(scriptsDir, { recursive: true });
  fs.mkdirSync(platBinDir, { recursive: true });

  fs.writeFileSync(
    path.join(pkgRoot, 'package.json'),
    JSON.stringify({ name: '@bitefoundry/onenote', version: '0.0.0-test' }),
    'utf8',
  );
  fs.writeFileSync(path.join(binDir, 'onenote.js'), '#!/usr/bin/env node\n// stub launcher\n', 'utf8');
  fs.copyFileSync(POSTINSTALL_SCRIPT, path.join(scriptsDir, 'postinstall.mjs'));

  fs.writeFileSync(
    path.join(platPkgDir, 'package.json'),
    JSON.stringify({ name: `@bitefoundry/${platPkgName}`, version: '0.0.0-test' }),
    'utf8',
  );
  const binaryPath = path.join(platBinDir, binaryName);
  fs.writeFileSync(binaryPath, 'FAKE-ONENOTE-NATIVE-BINARY', 'utf8');

  return { pkgRoot, binDir, scriptsDir, binaryPath };
}

test('runPostinstall skips development repo containing Cargo.toml', () => {
  const res = runPostinstall({ pkgRoot: REPO_ROOT });
  assert.deepEqual(res, { skipped: true, reason: 'dev_repo' });
});

test('runPostinstall links native binary and rewrites Windows shims in isolated install', () => {
  const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'onenote-postinstall-win-'));
  try {
    const { pkgRoot } = createFakeInstalledPkg(tmpRoot, { platform: 'win32', arch: 'x64' });
    const shimDir = path.join(tmpRoot, 'node_modules', '.bin');
    fs.mkdirSync(shimDir, { recursive: true });
    const cmdShim = path.join(shimDir, 'onenote.cmd');
    fs.writeFileSync(
      cmdShim,
      '@ECHO off\r\nnode "%~dp0\\..\\@bitefoundry\\onenote\\bin\\onenote.js" %*\r\n',
      'utf8',
    );

    const res = runPostinstall({ pkgRoot, platform: 'win32', arch: 'x64', env: {} });
    assert.equal(res.skipped, false);
    assert.equal(res.linked, 1);
    assert.equal(res.shimsRewritten, 1);
    assert.equal(
      fs.readFileSync(path.join(pkgRoot, 'bin', 'onenote.exe'), 'utf8'),
      'FAKE-ONENOTE-NATIVE-BINARY',
    );
    assert.match(fs.readFileSync(cmdShim, 'utf8'), /onenote\.exe/);
  } finally {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
});

test('postinstall.mjs executes main entrypoint when invoked through a symlinked file path', (t) => {
  const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'onenote-postinstall-symlink-'));
  try {
    const { pkgRoot } = createFakeInstalledPkg(tmpRoot);
    const realScript = path.join(pkgRoot, 'scripts', 'postinstall.mjs');
    const symlinkScript = path.join(tmpRoot, 'linked-postinstall.mjs');

    try {
      fs.symlinkSync(realScript, symlinkScript);
    } catch (err) {
      if (process.platform === 'win32' && err && err.code === 'EPERM') {
        t.skip('Windows file symlink creation requires Developer Mode or elevated privileges (EPERM)');
        return;
      }
      throw err;
    }

    const child = spawnSync(process.execPath, [symlinkScript], {
      cwd: tmpRoot,
      encoding: 'utf8',
    });
    assert.equal(child.status, 0, `postinstall exited ${child.status}: ${child.stderr}`);

    if (process.platform === 'win32') {
      const destExe = path.join(pkgRoot, 'bin', 'onenote.exe');
      assert.ok(fs.existsSync(destExe), 'expected postinstall via symlink to create bin/onenote.exe');
      assert.equal(fs.readFileSync(destExe, 'utf8'), 'FAKE-ONENOTE-NATIVE-BINARY');
    } else {
      const destJs = path.join(pkgRoot, 'bin', 'onenote.js');
      assert.equal(fs.readFileSync(destJs, 'utf8'), 'FAKE-ONENOTE-NATIVE-BINARY');
    }
  } finally {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
});

test('postinstall.mjs executes main entrypoint when invoked through a symlinked/junction directory', (t) => {
  const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'onenote-postinstall-dirlink-'));
  try {
    const { pkgRoot } = createFakeInstalledPkg(tmpRoot);
    const linkedPkgDir = path.join(tmpRoot, 'linked-pkg');

    try {
      fs.symlinkSync(pkgRoot, linkedPkgDir, process.platform === 'win32' ? 'junction' : 'dir');
    } catch (err) {
      if (process.platform === 'win32' && err && err.code === 'EPERM') {
        t.skip('Windows symlink creation failed with EPERM');
        return;
      }
      throw err;
    }

    const linkedScript = path.join(linkedPkgDir, 'scripts', 'postinstall.mjs');
    const child = spawnSync(process.execPath, [linkedScript], {
      cwd: tmpRoot,
      encoding: 'utf8',
    });
    assert.equal(child.status, 0, `postinstall exited ${child.status}: ${child.stderr}`);

    if (process.platform === 'win32') {
      const destExe = path.join(pkgRoot, 'bin', 'onenote.exe');
      assert.ok(fs.existsSync(destExe), 'expected postinstall via dir link to create bin/onenote.exe');
      assert.equal(fs.readFileSync(destExe, 'utf8'), 'FAKE-ONENOTE-NATIVE-BINARY');
    } else {
      const destJs = path.join(pkgRoot, 'bin', 'onenote.js');
      assert.equal(fs.readFileSync(destJs, 'utf8'), 'FAKE-ONENOTE-NATIVE-BINARY');
    }
  } finally {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
});

test('bin/onenote.js prints actionable error when platform binary package is missing', () => {
  const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'onenote-launcher-missing-'));
  try {
    const launcherCopy = path.join(tmpRoot, 'onenote.js');
    fs.copyFileSync(BIN_LAUNCHER, launcherCopy);
    const child = spawnSync(process.execPath, [launcherCopy, '--version'], {
      cwd: tmpRoot,
      encoding: 'utf8',
    });
    assert.notEqual(child.status, 0);
    assert.match(child.stderr, /no prebuilt binary found/);
    assert.match(child.stderr, /@bitefoundry\/onenote-/);
  } finally {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
});
