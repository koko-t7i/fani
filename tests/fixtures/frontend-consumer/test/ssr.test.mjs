import test from 'node:test';
import assert from 'node:assert/strict';
import i18next from 'i18next';
import { loadResources } from '../resources.mjs';
import { routes, defaults, translator, render, escape } from '../src/app.mjs';

const { resources, hashes } = await loadResources();
console.log('Consumed candidate SHA-256:', hashes);
for (const path of routes) test(`SSR ${path}`, async () => {
  const locale = path.split('/')[1];
  const instance = await translator(i18next, resources, locale);
  for (const values of [defaults, { name: 'Bob', user: { name: '李雷' } }, { name: '<img src=x onerror="window.pwned=1">', user: { name: '<script>alert(1)</script>' } }]) {
    const html = render(instance, path, values);
    const prefix = locale === 'fr' ? 'FR: ' : '';
    assert.ok(html.includes(`${prefix}Hello ${escape(values.name)}; again ${escape(values.name)}; account ${escape(values.user.name)}.`));
    if (path.endsWith('account')) assert.ok(html.includes(`${prefix}Account ${escape(values.user.name)} belongs to ${escape(values.name)} and ${escape(values.name)}.`));
    assert.ok(html.includes('<pre id="empty"></pre>'));
    assert.ok(html.includes('<pre id="whitespace"> \t\n </pre>'));
    assert.ok(html.includes(`${prefix}Café says &quot;welcome&quot;.\nNext line.`));
    assert.doesNotMatch(html, /\{\{|\}\}|<img|<script>/);
  }
});
test('missing translation fails without fallback', async () => {
  const broken = structuredClone(resources);
  delete broken.fr.common.checkout.title;
  await assert.rejects(translator(i18next, broken, 'fr'), /Missing translation: fr:checkout.title/);
});
