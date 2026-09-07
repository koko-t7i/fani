import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { startServer } from '../server.mjs';
import { routes, defaults } from '../src/app.mjs';

test('initial HTTP documents with JavaScript disabled', { timeout: 30000 }, async () => {
  const server = await startServer();
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    console.log(JSON.stringify({ browser: browser.version(), javaScriptEnabled: false, hashes: server.hashes }));
    const context = await browser.newContext({ javaScriptEnabled: false });
    const page = await context.newPage();
    page.setDefaultTimeout(5000);
    for (const path of routes) {
      const response = await page.goto(server.url + path);
      assert.equal(response.status(), 200, path);
      assert.match(response.headers()['content-type'], /text\/html/);
      assert.equal(new URL(page.url()).pathname, path);
      const [, locale, section] = path.split('/');
      const prefix = locale === 'fr' ? 'FR: ' : '';
      assert.equal(await page.locator('html').getAttribute('lang'), locale, path);
      assert.equal(await page.locator('html').getAttribute('data-ready'), null, path);
      assert.equal(await page.locator('h1').textContent(), `${prefix}${section === 'checkout' ? 'Checkout' : 'Account'}`, path);
      assert.equal(await page.locator('#message').textContent(), section === 'checkout'
        ? `${prefix}Hello Alice; again Alice; account Élodie.`
        : `${prefix}Account Élodie belongs to Alice and Alice.`, path);
      assert.equal(await page.locator('#shared-message').textContent(), `${prefix}Hello Alice; again Alice; account Élodie.`, path);
      assert.deepEqual(await page.locator('li').allTextContents(), [`${prefix}First item`, `${prefix}Nested item`, '', '  '], path);
      assert.equal(await page.locator('#empty').textContent(), '', path);
      assert.equal(await page.locator('#whitespace').textContent(), ' \t\n ', path);
      assert.equal(await page.locator('#detail').textContent(), `${prefix}Café says "welcome".\nNext line.`, path);
      assert.equal(await page.locator('[name=name]').inputValue(), defaults.name, path);
      assert.equal(await page.locator('[name=user]').inputValue(), defaults.user.name, path);
      assert.doesNotMatch(await page.locator('main').textContent(), /\{\{|\}\}/, path);
    }
    await context.close();
  } finally {
    if (browser) await browser.close();
    await server.close();
  }
});

for (const mobile of [false, true]) test(`browser ${mobile ? 'mobile' : 'desktop'}`, { timeout: 90000 }, async () => {
  const server = await startServer();
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    console.log(JSON.stringify({ browser: browser.version(), mobile, hashes: server.hashes }));
    const context = await browser.newContext({ viewport: mobile ? { width: 390, height: 844 } : { width: 1280, height: 800 }, isMobile: mobile, hasTouch: mobile });
    assert.deepEqual((await (await context.request.get(server.url + '/resources.json')).json()).hashes, server.hashes);
    const page = await context.newPage();
    page.setDefaultTimeout(8000);
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
    page.on('requestfailed', request => errors.push(`${request.url()}: ${request.failure()?.errorText}`));
    page.on('response', response => { if (response.status() >= 400) errors.push(`${response.status()} ${response.url()}`); });
    async function verify(path, values) {
      await page.waitForFunction(expected => location.pathname === expected && document.documentElement.lang === expected.split('/')[1] && document.documentElement.dataset.ready === 'true', path);
      const locale = path.split('/')[1];
      const section = path.split('/')[2];
      const prefix = locale === 'fr' ? 'FR: ' : '';
      const shared = `${prefix}Hello ${values.name}; again ${values.name}; account ${values.user.name}.`;
      const message = section === 'checkout' ? shared : `${prefix}Account ${values.user.name} belongs to ${values.name} and ${values.name}.`;
      await page.waitForFunction(expected => document.querySelector('#message')?.textContent === expected, message);
      assert.equal(await page.locator('#shared-message').textContent(), shared);
      assert.equal(await page.locator('h1').textContent(), `${prefix}${section === 'checkout' ? 'Checkout' : 'Account'}`);
      assert.equal(await page.locator('[name=name]').inputValue(), values.name);
      assert.equal(await page.locator('[name=user]').inputValue(), values.user.name);
      assert.equal(await page.locator('#empty').textContent(), '');
      assert.equal(await page.locator('#whitespace').textContent(), ' \t\n ');
      assert.equal(await page.locator('#detail').textContent(), `${prefix}Café says "welcome".\nNext line.`);
      assert.deepEqual(await page.locator('li').allTextContents(), [`${prefix}First item`, `${prefix}Nested item`, '', '  ']);
      assert.doesNotMatch(await page.locator('main').textContent(), /\{\{|\}\}|shared\.(empty|detail)|checkout\.message/);
      assert.equal(await page.locator('main img, main script').count(), 0);
      assert.equal(await page.evaluate(() => window.pwned), undefined);
      assert.deepEqual(errors, [], `Browser errors on ${path}; hashes=${JSON.stringify(server.hashes)}`);
    }
    for (const path of routes) {
      await page.goto(server.url + path);
      await verify(path, defaults);
      await page.reload();
      await verify(path, defaults);
    }
    await page.goto(server.url + '/en/checkout');
    await verify('/en/checkout', defaults);
    for (const values of [{ name: '', user: { name: '' } }, { name: 'Bob', user: { name: '李雷' } }, { name: '<img src=x onerror="window.pwned=1">', user: { name: '<script>window.pwned=1</script>' } }]) {
      await page.locator('[name=name]').fill('');
      await page.locator('[name=name]').pressSequentially(values.name);
      await page.locator('[name=user]').fill('');
      await page.locator('[name=user]').pressSequentially(values.user.name);
      await page.getByRole('button', { name: 'Apply' }).click();
      await verify('/en/checkout', values);
      let previous = '/en/checkout';
      for (const path of ['/fr/checkout', '/fr/account', '/en/account', '/fr/account', '/fr/checkout', '/en/checkout', '/en/account', '/en/checkout']) {
        await page.locator(`a[href="${path}"]`).click();
        await verify(path, values);
        await page.goBack(); await verify(previous, values);
        await page.goForward(); await verify(path, values);
        previous = path;
      }
      await page.goBack(); await verify('/en/account', values);
      await page.goForward(); await verify('/en/checkout', values);
    }
    await page.reload(); await verify('/en/checkout', defaults);
    assert.equal((await context.request.get(server.url + '/missing')).status(), 404);
    await context.close();
  } finally {
    if (browser) await browser.close();
    await server.close();
  }
});
