import { languageWall } from './language-wall.js';
import { mountLoadingOrb } from './loading-orb.js';
import { bindSpringRange } from './range-motion.js';
import { languageNames, localeFor, locales, setupLabels, updateLabels } from './setup-i18n.js';

const $ = id => document.getElementById(id);
const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args);
const updateControl = $('startup-update');
const updateTrigger = updateControl.querySelector('summary');
const languageKey = 'setup-language';
const savedLanguage = localStorage.getItem(languageKey);
let choosingLanguage = !locales.includes(savedLanguage);
let locale = choosingLanguage ? localeFor(navigator.language) : savedLanguage;
let startup = { phase: window.__AIMANITY_STARTUP__ === true ? 'checking' : '', error: '' };
let polling = false;
let snapshot, actionError = '', statusError = '', pending = false, pollTimer, previousView, stopped = false;
let activity = [], visibleActivityKey = '';
let updateOffer, updating = false, updateFailed = false, updatePinned = false;
const resources = { phone: { cpus: 2, memory_mb: 2048, storage_gb: 16 }, desktop: { cpus: 2, memory_mb: 2560 } };
const sliders = [];
const startupLabels = { checking: 'startupChecking', repairing: 'startupRepairing', opening: 'startupOpening', failed: 'startupFailed' };
const activityLabels = { running: 'activityRunning', completed: 'activityCompleted', failed: 'activityFailed' };
const errorLabels = {
  host_resources: 'hostResources', host_backend_pending: 'hostPending', installation_cancelled: 'cancelled',
  command_line_tools: 'commandLineTools', payload_unreadable: 'payloadUnreadable', invalid_payload: 'invalidPayload', download_failed: 'downloadFailed',
  download_checksum: 'downloadFailed', installation_interrupted: 'interrupted', wsl_initialization_unknown: 'interrupted',
  invalid_install_checkpoint: 'invalidCheckpoint', checkpoint_write_failed: 'checkpointFailed',
  foreign_installation: 'foreignInstallation', workbench_unavailable: 'workbenchUnavailable',
};
const label = key => setupLabels[locale][key];
const showError = (node, code) => {
  node.hidden = !code;
  node.textContent = code ? label(errorLabels[code] || 'failed') : '';
};

function translate() {
  document.documentElement.lang = locale;
  document.documentElement.dir = locale === 'ar' ? 'rtl' : 'ltr';
  for (const node of document.querySelectorAll('[data-i18n]')) node.textContent = label(node.dataset.i18n);
}

function renderActivity(entries) {
  const list = $('install-activity');
  list.hidden = entries.length === 0;
  const running = entries.findLast(entry => entry.state === 'running');
  const recent = entries.slice(-3);
  const rows = running && !recent.includes(running) ? [running, ...recent.slice(-2)] : recent;
  const key = JSON.stringify([locale, rows]);
  if (key === visibleActivityKey) return;
  visibleActivityKey = key;
  const current = running || rows.at(-1);
  list.replaceChildren(...rows.map(entry => {
    const row = document.createElement('li');
    row.dataset.state = entry.state;
    row.toggleAttribute('data-current', entry === current);
    const command = document.createElement('code');
    command.textContent = entry.command;
    const result = document.createElement('span');
    result.dir = 'auto';
    result.textContent = label(activityLabels[entry.state]);
    row.append(command, result);
    return row;
  }));
}

function renderUpdate() {
  updateControl.hidden = !startup.phase || !updateOffer;
  if (updateControl.hidden) return;
  const [name, confirm, inProgress, interrupted] = updateLabels[locale];
  $('startup-update-label').textContent = name;
  $('startup-update-release').textContent = `${updateOffer.version} ${updateOffer.name}`;
  $('startup-update-notes').textContent = updateOffer.notes;
  const error = $('startup-update-error');
  error.hidden = !updateFailed;
  error.textContent = updateFailed ? interrupted : '';
  const button = $('startup-update-install');
  button.disabled = updating;
  button.textContent = updating ? inProgress : confirm;
}

function render() {
  const startupPhase = startup.phase && statusError ? 'failed' : startup.phase;
  const selectingLanguage = choosingLanguage && !startupPhase;
  $('language-screen').hidden = !selectingLanguage;
  $('setup-flow').hidden = selectingLanguage;
  languages.update(locale, selectingLanguage);
  const progress = snapshot?.progress;
  const state = progress?.state;
  const active = state === 'running' || state === 'license';
  const ready = state === 'ready';
  const installed = state === 'installed';
  const showingProgress = !!(startupPhase || state);
  const starting = !!startupPhase && startupPhase !== 'failed';
  orb.setRunning(!selectingLanguage && (starting || (!startupPhase && state === 'running')));
  $('local-panel').hidden = showingProgress;
  $('progress').hidden = !showingProgress;
  $('progress').dataset.startup = String(!!startupPhase);
  renderActivity(!selectingLanguage && showingProgress && !ready && state !== 'license' ? activity : []);
  renderUpdate();
  if (startupPhase) {
    $('progress-detail').dataset.state = starting ? 'running' : 'startup-failed';
    $('progress-title').textContent = label('start');
    $('phase').textContent = (state === 'running' && label(progress.phase)) || label(startupLabels[startupPhase]);
    showError($('progress-error'), actionError || statusError || startup.error);
    $('license').hidden = true;
    $('retry').hidden = starting;
    $('retry').textContent = label('retry');
    $('cancel').hidden = true;
    $('accept').hidden = true;
    $('enter').hidden = true;
  } else if (state) {
    $('progress-detail').dataset.state = state;
    $('progress-title').textContent = label(ready ? 'ready' : 'installStep');
    $('phase').textContent = label(ready ? 'readyDevices' : progress.phase === 'failed' ? (state === 'cancelled' ? 'stopped' : 'installFailed') : progress.phase) || label('busy');
    showError($('progress-error'), actionError || statusError || (state === 'failed' || state === 'blocked' ? progress.error || 'installation_failed' : ''));
    $('license').hidden = state !== 'license';
    if ($('license').textContent !== progress.license) $('license').textContent = progress.license;
    $('retry').hidden = !['failed', 'blocked', 'cancelled', 'interrupted', 'installed'].includes(state);
    $('retry').textContent = label(installed ? 'start' : 'retry');
    $('cancel').hidden = !active;
    $('accept').hidden = state !== 'license';
    $('enter').hidden = !ready;
  } else if (!selectingLanguage) {
    const host = snapshot?.host;
    const checks = snapshot?.checks ?? [];
    $('host-name').textContent = host ? (host.platform === 'macos' ? 'macOS · Apple Silicon' : host.platform === 'windows' ? 'Windows 11' : host.platform) : '';
    const capacity = new Intl.NumberFormat(locale, { style: 'unit', unit: 'gigabyte', unitDisplay: 'short' });
    $('checks').replaceChildren(...checks.map(({ name, passed, value, required }) => {
      const node = document.createElement('span');
      node.classList.toggle('failed', !passed);
      const title = label(name === 'memory' ? 'memoryCheck' : 'storageCheck');
      node.textContent = `${title} ${capacity.format(value)}${passed ? '' : ` / ${capacity.format(required)}`}`;
      return node;
    }));
    const insufficient = checks.some(check => !check.passed);
    showError($('host-error'), actionError || statusError || (host && (!host.implemented ? 'host_backend_pending' : insufficient ? 'host_resources' : '')));
    $('install').disabled = !host?.implemented || insufficient || pending;
  }
  for (const button of $('progress').querySelectorAll('button')) button.disabled = pending;
  for (const { input } of sliders) input.disabled = pending;
  const view = selectingLanguage ? $('language-screen') : showingProgress ? $('progress') : $('local-panel');
  if (view !== previousView && previousView) {
    view.querySelector('h1').focus({ preventScroll: true });
    if (!matchMedia('(prefers-reduced-motion: reduce)').matches) {
      view.animate([
        { opacity: .35, transform: 'translateY(12px) scale(.96)' },
        { opacity: 1, transform: 'none' },
      ], { duration: 480, easing: 'cubic-bezier(.2, 1.12, .35, 1)' });
    }
  }
  previousView = view;
}

const languages = languageWall($('language-wall'), locales, languageNames, value => {
  locale = value;
  translate();
  render();
  languages.focus();
});
const orb = mountLoadingOrb($('install-orb'));
window.addEventListener('pagehide', orb.dispose, { once: true });

$('language-continue').addEventListener('click', () => {
  localStorage.setItem(languageKey, locale);
  choosingLanguage = false;
  render();
  beginPolling();
});
function resourceSlider(device, key, values) {
  const field = document.createElement('div');
  field.className = 'resource-slider';
  const id = `${device}-${key}`;
  const caption = document.createElement('div');
  caption.className = 'resource-caption';
  const title = document.createElement('label');
  title.htmlFor = id;
  title.dataset.i18n = { cpus: 'cpu', memory_mb: 'memory', storage_gb: 'storage' }[key];
  const output = document.createElement('output');
  output.setAttribute('for', id);
  output.setAttribute('aria-hidden', 'true');
  const input = document.createElement('input');
  input.id = id;
  input.type = 'range';
  input.min = '0';
  input.max = String(values.length - 1);
  input.step = '1';
  const range = document.createElement('div');
  range.className = 'spring-range';
  const rail = document.createElement('span');
  rail.className = 'spring-range__rail';
  rail.setAttribute('aria-hidden', 'true');
  const thumb = document.createElement('span');
  thumb.className = 'spring-range__thumb';
  rail.append(thumb);
  range.append(input, rail);
  window.addEventListener('pagehide', bindSpringRange(input, thumb), { once: true });
  const names = values.map(value => key === 'cpus' ? String(value) : `${key === 'memory_mb' ? value / 1024 : value} GB`);
  function update(value) {
    const index = values.indexOf(value);
    input.value = String(index);
    range.style.setProperty('--range-progress', `${index / (values.length - 1) * 100}%`);
    input.setAttribute('aria-valuetext', names[index]);
    output.value = names[index];
  }
  input.addEventListener('input', () => {
    resources[device][key] = values[Number(input.value)];
    update(resources[device][key]);
  });
  update(resources[device][key]);
  sliders.push({ input, update, device, key });
  caption.append(title, output);
  field.append(caption, range);
  $(`${device}-fields`).append(field);
}
resourceSlider('phone', 'cpus', [1, 2, 4]);
resourceSlider('phone', 'memory_mb', [1536, 2048, 3072, 4096]);
resourceSlider('phone', 'storage_gb', [16, 24, 32]);
resourceSlider('desktop', 'cpus', [1, 2, 4]);
resourceSlider('desktop', 'memory_mb', [2048, 2560, 3072, 4096]);

async function refresh() {
  try {
    const next = await invoke('deployment_status');
    startup = next.startup;
    activity = next.activity ?? [];
    if (!startup.phase) {
      if (!snapshot && next.previous_request) {
        for (const { update, device, key } of sliders) {
          resources[device][key] = next.previous_request[device][key];
          update(resources[device][key]);
        }
      }
      snapshot = next;
    } else {
      snapshot = next.progress ? next : null;
    }
    statusError = '';
  } catch (error) {
    statusError = String(error);
  }
  render();
}

function scheduleRefresh() {
  if (stopped || (choosingLanguage && !startup.phase)) {
    polling = false;
    return;
  }
  const state = snapshot?.progress.state;
  pollTimer = setTimeout(async () => {
    if (!pending) await refresh();
    scheduleRefresh();
  }, startup.phase && startup.phase !== 'failed' ? 300 : state === 'running' || state === 'license' ? 800 : 3000);
}

function beginPolling() {
  if (polling) return;
  polling = true;
  void refresh().then(scheduleRefresh);
}

async function run(command, args) {
  if (pending) return;
  pending = true;
  actionError = '';
  render();
  try {
    await invoke(command, args);
  } catch (error) {
    actionError = String(error);
  } finally {
    pending = false;
    await refresh();
  }
}
const start = () => run('start_deployment', { request: { locale, ...resources } });
$('local-setup').addEventListener('submit', event => { event.preventDefault(); void start(); });
$('retry').addEventListener('click', () => startup.phase ? void run('retry_startup') : void start());
$('cancel').addEventListener('click', () => void run('cancel_deployment'));
$('accept').addEventListener('click', () => void run('accept_deployment_license'));
$('enter').addEventListener('click', () => void run('open_workbench', { locale }));
updateTrigger.addEventListener('pointerenter', event => {
  if (event.pointerType === 'mouse') updateControl.open = true;
});
updateControl.addEventListener('pointerleave', () => {
  if (!updatePinned) updateControl.open = false;
});
updateTrigger.addEventListener('click', event => {
  event.preventDefault();
  updateControl.open = updatePinned = !updatePinned;
});
updateControl.addEventListener('keydown', event => {
  if (event.key !== 'Escape' || !updateControl.open) return;
  event.preventDefault();
  updateControl.open = updatePinned = false;
  updateTrigger.focus();
});
$('startup-update-install').addEventListener('click', async () => {
  updatePinned = true;
  updating = true;
  updateFailed = false;
  renderUpdate();
  try {
    await invoke('install_client_update', { version: updateOffer.version });
  } catch {
    updating = false;
    updateFailed = true;
    renderUpdate();
  }
});

window.addEventListener('pagehide', () => {
  stopped = true;
  clearTimeout(pollTimer);
});
translate();
render();
if (startup.phase) {
  void invoke('check_client_update').then(offer => {
    updateOffer = offer;
    renderUpdate();
  }, () => {});
}
if (startup.phase || !choosingLanguage) beginPolling();
