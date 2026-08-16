const fs = require('fs');
const path = require('path');

const root = path.join(__dirname, '..');
const packageJson = JSON.parse(
  fs.readFileSync(path.join(root, 'package.json'), 'utf8'),
);
const tauriConf = JSON.parse(
  fs.readFileSync(path.join(root, 'src-tauri', 'tauri.conf.json'), 'utf8'),
);
const cargoToml = fs.readFileSync(
  path.join(root, 'src-tauri', 'Cargo.toml'),
  'utf8',
);
const cargoVersion = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)[1];
const version = packageJson.version;
const releaseTag = process.env.RELEASE_TAG;
const expected = `v${version}`;

if (version !== tauriConf.version || version !== cargoVersion) {
  throw new Error(
    'package.json / tauri.conf.json / Cargo.toml versions must match.',
  );
}

if (releaseTag && releaseTag !== expected) {
  throw new Error(`Tag must be ${expected}, got ${releaseTag}.`);
}

console.log(`Version check passed: ${version}${releaseTag ? ` (${releaseTag})` : ''}`);
