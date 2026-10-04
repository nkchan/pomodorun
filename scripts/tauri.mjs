import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
const env = { ...process.env };
const local = resolve('.toolchain');
if (existsSync(`${local}/cargo/bin/cargo`)) {
  env.CARGO_HOME = `${local}/cargo`;
  env.RUSTUP_HOME = `${local}/rustup`;
  env.PATH = `${local}/cargo/bin:${env.PATH}`;
}
const result = spawnSync(resolve('node_modules/.bin/tauri'), process.argv.slice(2), { env, stdio: 'inherit' });
process.exit(result.status ?? 1);
