import { defaults, translator, render, routes } from '/app.mjs';

const response = await fetch('/resources.json');
if (!response.ok) throw new Error(`Resources HTTP ${response.status}`);
const { resources } = await response.json();
let values = structuredClone(defaults);
const instance = await translator(window.i18next, resources, location.pathname.split('/')[1]);
async function update() {
  if (!routes.includes(location.pathname)) throw new Error(`Unknown route: ${location.pathname}`);
  const locale = location.pathname.split('/')[1];
  await instance.changeLanguage(locale);
  document.documentElement.lang = locale;
  document.querySelector('main').innerHTML = render(instance, location.pathname, values);
}
document.addEventListener('click', async event => {
  const link = event.target.closest('a[data-route]');
  if (!link || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  event.preventDefault();
  history.pushState(null, '', link.href);
  await update();
});
document.addEventListener('input', event => {
  if (event.target.name === 'name') values.name = event.target.value;
  if (event.target.name === 'user') values.user.name = event.target.value;
});
document.addEventListener('submit', async event => {
  event.preventDefault();
  await update();
});
window.addEventListener('popstate', update);
await update();
document.documentElement.dataset.ready = 'true';
