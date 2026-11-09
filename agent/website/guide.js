import { initI18n, getLocale, onLocaleChange, t } from './i18n.js?v=20261014-language-fix';

const $ = (sel, scope = document) => scope.querySelector(sel);
const $$ = (sel, scope = document) => [...scope.querySelectorAll(sel)];

const GUIDES = [
  { id: 'user-guide', file: { zh: './content/user-guide.html', en: './content/user-guide-en.html' }, title: { zh: '使用手册', en: 'User Guide' } },
  { id: 'extension-authoring', file: './content/extension-authoring.html', title: { zh: '扩展开发', en: 'Extension Authoring' } },
  { id: 'agent-project', file: './content/agent-project.html', title: { zh: 'Agent 项目结构', en: 'Agent Project Structure' } },
];

let currentGuide = null;
let guideLoadId = 0;
const htmlCache = {};

function bindMenu() {
  const toggle = $('[data-menu-toggle]');
  const menu = $('[data-mobile-menu]');
  toggle.addEventListener('click', () => {
    const open = menu.classList.toggle('is-open');
    toggle.setAttribute('aria-expanded', String(open));
  });
  $$('a', menu).forEach((link) => link.addEventListener('click', () => {
    menu.classList.remove('is-open');
    toggle.setAttribute('aria-expanded', 'false');
  }));
}

function bindScrollEffects() {
  const header = $('[data-sticky-header]');
  const marker = document.createElement('div');
  marker.className = 'scroll-marker';
  document.body.prepend(marker);
  new IntersectionObserver(([entry]) => header.classList.toggle('is-scrolled', !entry.isIntersecting), { threshold: 0 }).observe(marker);
}

function renderToc() {
  const nav = $('[data-guide-toc]');
  const headings = $$('[data-guide-body] h2, [data-guide-body] h3');
  nav.innerHTML = headings.map((heading) => `<a href="#${heading.id}" class="level-${heading.tagName.slice(1)}">${heading.textContent}</a>`).join('');
  $$('a', nav).forEach((link) => link.addEventListener('click', (event) => {
    event.preventDefault();
    const id = link.getAttribute('href').slice(1);
    document.getElementById(id)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    history.replaceState(null, '', `#${id}`);
  }));
}

function updateTocHighlight() {
  const headings = $$('[data-guide-body] h2, [data-guide-body] h3');
  const tocLinks = $$('[data-guide-toc] a');
  if (!headings.length || !tocLinks.length) return;
  let activeId = null;
  const scrollTop = window.scrollY + 150;
  headings.forEach((heading) => { if (heading.offsetTop <= scrollTop) activeId = heading.id; });
  tocLinks.forEach((link) => link.classList.toggle('is-active', link.getAttribute('href').slice(1) === activeId));
}

async function loadGuide(guideId) {
  const loadId = ++guideLoadId;
  const guide = GUIDES.find((item) => item.id === guideId) || GUIDES[0];
  currentGuide = guide;
  const locale = getLocale();
  const file = typeof guide.file === 'string' ? guide.file : (guide.file[locale] || guide.file.zh);
  const title = guide.title[locale] || guide.title.zh;
  $$('[data-guide-title]').forEach((element) => { element.textContent = title; });
  document.title = `${title} · rpi`;

  const selector = $('[data-guide-selector]');
  selector.innerHTML = GUIDES.map((item) => `<button type="button" data-guide-id="${item.id}" class="${item.id === guide.id ? 'is-active' : ''}">${item.title[locale] || item.title.zh}</button>`).join('');
  $$('[data-guide-id]', selector).forEach((button) => button.addEventListener('click', () => {
    const url = new URL(window.location.href);
    url.searchParams.set('doc', button.dataset.guideId);
    url.hash = '';
    history.replaceState(null, '', `${url.pathname}${url.search}`);
    loadGuide(button.dataset.guideId);
  }));

  const body = $('[data-guide-body]');
  body.innerHTML = `<div class="guide-loading">${t('guide.loading')}</div>`;
  try {
    if (!htmlCache[file]) {
      const response = await fetch(file);
      if (!response.ok) throw new Error(`Failed to load ${file}: ${response.status}`);
      htmlCache[file] = await response.text();
    }
    if (loadId !== guideLoadId) return;
    body.innerHTML = htmlCache[file];
    renderToc();
    window.scrollTo({ top: 0, behavior: 'smooth' });
    updateTocHighlight();
  } catch (error) {
    if (loadId !== guideLoadId) return;
    console.error(error);
    body.innerHTML = `<div class="guide-error">${t('guide.error')}</div>`;
  }
}

function getGuideFromUrl() {
  return new URLSearchParams(window.location.search).get('doc') || 'user-guide';
}

async function init() {
  await initI18n();
  bindMenu();
  bindScrollEffects();
  await loadGuide(getGuideFromUrl());
  window.addEventListener('scroll', updateTocHighlight, { passive: true });
  onLocaleChange(() => currentGuide && loadGuide(currentGuide.id));
}

init().catch((error) => {
  console.error('Guide init failed:', error);
  $('[data-guide-body]').innerHTML = `<div class="guide-error">${t('guide.error')}</div>`;
});
