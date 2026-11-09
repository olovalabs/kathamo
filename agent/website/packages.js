import { getLocale, initI18n, localizeData, onLocaleChange, t } from './i18n.js?v=20261014-language-fix';

const $ = (selector, scope = document) => scope.querySelector(selector);
const $$ = (selector, scope = document) => [...scope.querySelectorAll(selector)];
const state = { rawPackages: [], packages: [], metadata: null, query: '', type: 'extension', sort: 'recent' };

function showToast(message) {
  const toast = $('[data-toast]'); toast.textContent = message; toast.classList.add('is-visible');
  clearTimeout(showToast.timer); showToast.timer = setTimeout(() => toast.classList.remove('is-visible'), 2200);
}

function bindMenu() {
  const toggle = $('[data-menu-toggle]'); const menu = $('[data-mobile-menu]');
  toggle.addEventListener('click', () => { const open = menu.classList.toggle('is-open'); toggle.setAttribute('aria-expanded', String(open)); });
  $$('a', menu).forEach((link) => link.addEventListener('click', () => { menu.classList.remove('is-open'); toggle.setAttribute('aria-expanded', 'false'); }));
}

function bindScrollEffects() {
  const header = $('[data-sticky-header]'); const marker = document.createElement('div'); marker.className = 'scroll-marker'; document.body.prepend(marker);
  new IntersectionObserver(([entry]) => header.classList.toggle('is-scrolled', !entry.isIntersecting), { threshold: 0 }).observe(marker);
}

function sortedPackages() {
  const query = state.query.trim().toLowerCase();
  return state.packages.filter((item) => {
    const haystack = `${item.name} ${item.description} ${item.author}`.toLowerCase();
    return (state.type === 'all' || item.type === state.type) && (!query || haystack.includes(query));
  }).sort((a, b) => {
    if (state.sort === 'name') return a.name.localeCompare(b.name);
    if (state.sort === 'recent') return a.age - b.age;
    return b.downloads - a.downloads;
  });
}

function packageCard(item) {
  const downloadLabel = item.downloadsLabel;
  const downloadTitle = t('packages.downloadExplanation', { count: item.downloadsLabel });
  return `<article class="package-card"><div class="package-card-top"><span class="package-type type-${item.type}">${t(`packages.type.${item.type}`)}</span><span class="package-age">${item.ageLabel}</span></div><h3><a href="${item.repository}" target="_blank" rel="noreferrer">${item.name} <span aria-hidden="true">↗</span></a></h3><p>${item.description}</p><div class="package-card-bottom"><span class="package-author">${item.author}</span><span class="package-downloads" title="${downloadTitle}"><svg class="download-icon" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3v12m-5-5 5 5 5-5M5 16v4h14v-4"/></svg><span>${downloadLabel}</span></span></div><div class="package-command"><code>${item.install}</code><button type="button" aria-label="${t('packages.copyInstall', { name: item.name })}" data-copy-package="${item.install}">⧉</button></div></article>`;
}

function renderRecent() {
  const recent = state.packages.filter((item) => item.type === 'extension')
    .sort((a, b) => a.age - b.age)
    .slice(0, 4);
  $('[data-recent-packages]').innerHTML = recent.map((item) => `<a class="recent-package" href="${item.repository}" target="_blank" rel="noreferrer"><strong>${item.name}</strong><span>${item.description}</span><small>${item.ageLabel}</small></a>`).join('');
}

function renderList() {
  const items = sortedPackages();
  $('[data-package-count]').textContent = state.packages.length;
  $('[data-results-count]').textContent = t('packages.results', { count: items.length });
  $('[data-package-list]').innerHTML = items.map(packageCard).join('');
  $('[data-empty-state]').hidden = items.length > 0;
  const date = new Intl.DateTimeFormat(getLocale() === 'zh' ? 'zh-CN' : 'en', { dateStyle: 'medium', timeZone: 'Asia/Shanghai' }).format(new Date(state.metadata.checkedAt));
  $('[data-catalog-updated]').textContent = t('packages.updatedAt', { date, extensions: state.metadata.extensionCount, crates: state.metadata.coreCrateCount });
  $$('[data-copy-package]').forEach((button) => button.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(button.dataset.copyPackage); showToast(t('packages.copySuccess')); } catch { showToast(t('copy.failed')); }
  }));
}

async function init() {
  await initI18n();
  const responses = await Promise.all(['./data/packages.json', './data/packages-meta.json'].map(async (url) => {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`Failed to load ${url}: ${response.status}`);
    return response.json();
  }));
  [state.rawPackages, state.metadata] = responses;
  state.packages = localizeData(state.rawPackages, 'packages');
  renderRecent(); renderList(); bindMenu(); bindScrollEffects();
  $('[data-package-search]').addEventListener('input', (event) => { state.query = event.target.value; renderList(); });
  $('[data-package-type]').addEventListener('change', (event) => { state.type = event.target.value; renderList(); });
  $('[data-package-sort]').addEventListener('change', (event) => { state.sort = event.target.value; renderList(); });
  $('[data-reset]').addEventListener('click', () => { state.query = ''; state.type = 'extension'; state.sort = 'recent'; $('[data-package-search]').value = ''; $('[data-package-type]').value = 'extension'; $('[data-package-sort]').value = 'recent'; renderList(); });
  $$('[data-copy-command]').forEach((button) => button.addEventListener('click', async () => { try { await navigator.clipboard.writeText(button.previousElementSibling.textContent); showToast(t('copy.commandSuccess')); } catch { showToast(t('copy.failed')); } }));
  onLocaleChange(() => { state.packages = localizeData(state.rawPackages, 'packages'); renderRecent(); renderList(); });
}

init().catch((error) => { console.error(error); showToast(t('error.data')); });
