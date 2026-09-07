export const routes = ['/en/checkout', '/fr/checkout', '/en/account', '/fr/account'];
export const defaults = { name: 'Alice', user: { name: 'Élodie' } };
export const keys = ['checkout.title', 'checkout.message', 'account.title', 'account.message', 'shared.empty', 'shared.whitespace', 'shared.detail', 'shared.items.0', 'shared.items.1.label', 'shared.items.2', 'shared.items.3'];
export const escape = value => String(value).replace(/[&<>"'\/]/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;', '/': '&#x2F;' })[char]);

export async function translator(i18next, resources, lng) {
  const instance = i18next.createInstance();
  // Static text and dynamic parameters are escaped separately, exactly once.
  const safe = value => typeof value === 'string' ? escape(value) : Array.isArray(value) ? value.map(safe)
    : value && typeof value === 'object' ? Object.fromEntries(Object.entries(value).map(([key, child]) => [key, safe(child)])) : value;
  await instance.init({ lng, fallbackLng: false, supportedLngs: ['en', 'fr'], resources: safe(resources),
    defaultNS: 'common', returnEmptyString: true, interpolation: { escapeValue: true } });
  for (const key of keys) {
    if (!instance.exists(key)) throw new Error(`Missing translation: ${lng}:${key}`);
  }
  return instance;
}

export function render(instance, path, values = defaults) {
  if (!routes.includes(path)) throw new Error(`Unknown route: ${path}`);
  const [, locale, page] = path.split('/');
  const text = key => {
    if (!instance.exists(key)) throw new Error(`Missing translation: ${locale}:${key}`);
    return instance.t(key, values);
  };
  return `<h1>${text(`${page}.title`)}</h1>
    <nav><a data-route href="/${locale}/checkout">Checkout</a> <a data-route href="/${locale}/account">Account</a>
    <a data-route href="/en/${page}">English</a> <a data-route href="/fr/${page}">Français</a></nav>
    <form><label>Name <input name="name" value="${escape(values.name)}"></label>
    <label>User name <input name="user" value="${escape(values.user.name)}"></label><button>Apply</button></form>
    <p id="message">${text(`${page}.message`)}</p><p id="shared-message">${text('checkout.message')}</p>
    <pre id="empty">${text('shared.empty')}</pre><pre id="whitespace">${text('shared.whitespace')}</pre>
    <pre id="detail">${text('shared.detail')}</pre>
    <ul>${['shared.items.0', 'shared.items.1.label', 'shared.items.2', 'shared.items.3'].map(key => `<li>${text(key)}</li>`).join('')}</ul>`;
}
