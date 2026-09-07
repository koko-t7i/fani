import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { loadResources, expectedTarget } from '../resources.mjs';
import { startServer } from '../server.mjs';

test('synthetic harness self-test ONLY, not fani format acceptance', async () => {
  const root = await mkdtemp(join(tmpdir(), 'fani-consumer-harness-'));
  const oldHashes = process.env.FANI_CONSUMER_HASHES;
  delete process.env.FANI_CONSUMER_HASHES;
  try {
    await assert.rejects(loadResources(''), /FANI_CONSUMER_ROOT/);
    await assert.rejects(loadResources('relative'), /absolute/);
    await mkdir(join(root, 'messages/en'), { recursive: true });
    await mkdir(join(root, 'messages/fr'), { recursive: true });
    const source = await readFile(new URL('../seed/messages/en/common.json', import.meta.url));
    await writeFile(join(root, 'messages/en/common.json'), source);
    await assert.rejects(startServer({ root }), /ENOENT/);
    const targetPath = join(root, 'messages/fr/common.json');
    await writeFile(targetPath, '{');
    await assert.rejects(loadResources(root), SyntaxError);
    const target = expectedTarget(JSON.parse(source));
    for (const mutate of [
      value => { delete value.checkout.title; },
      value => { value.checkout.message = 'FR: missing parameters'; },
      value => { value.shared.count = 43; },
      value => { value.shared.empty = 'FR: '; },
      value => { value.shared.items.reverse(); },
    ]) {
      const broken = structuredClone(target); mutate(broken);
      await writeFile(targetPath, JSON.stringify(broken));
      await assert.rejects(loadResources(root), /provider convention/);
    }
    await writeFile(targetPath, JSON.stringify(target));
    const { hashes } = await loadResources(root);
    const manifest = join(root, 'hashes.json');
    await writeFile(manifest, JSON.stringify(hashes));
    process.env.FANI_CONSUMER_HASHES = manifest;
    await loadResources(root);
    await writeFile(targetPath, JSON.stringify(target) + '\n');
    await assert.rejects(loadResources(root), /SHA-256 mismatch/);
    await writeFile(targetPath, JSON.stringify(target));
    const server = await startServer({ root });
    try {
      assert.equal((await fetch(`${server.url}/unknown`)).status, 404);
      assert.equal((await fetch(`${server.url}/en/checkout`, { method: 'POST' })).status, 405);
      assert.equal((await fetch(`${server.url}/en/checkout`)).status, 200);
      assert.deepEqual((await (await fetch(`${server.url}/resources.json`)).json()).hashes, hashes);
    } finally { await server.close(); }
  } finally {
    if (oldHashes === undefined) delete process.env.FANI_CONSUMER_HASHES;
    else process.env.FANI_CONSUMER_HASHES = oldHashes;
    await rm(root, { recursive: true, force: true });
  }
});
