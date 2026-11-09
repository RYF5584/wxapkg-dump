/* ============================================================================
 * wxapkg-dump — desktop UI
 *
 * No framework, no build step. Five rules keep it predictable:
 *   1. DOM is built with createElement/textContent only, so text from the engine
 *      is escaped by construction;
 *   2. every capability goes through call(), which wraps one Tauri command;
 *   3. long operations report progress through the wxapkg-job event, and every
 *      wait shows a skeleton or a busy control;
 *   4. one page, two views; anything that needs room is a modal;
 *   5. results are announced through notify(), never through a bare string.
 * ========================================================================== */

'use strict';

const TAURI = window.__TAURI__ || {};
const call = (command, args) => TAURI.core.invoke(command, args || {});

const THEME_STORAGE = 'wxapkg-dump.theme';
const LANG_STORAGE = 'wxapkg-dump.lang';
const DOWNLOADS_STORAGE = 'wxapkg-dump.downloads';

/** Reads the download history, newest first, tolerating a corrupt store. */
function loadDownloads() {
  try { return JSON.parse(localStorage.getItem(DOWNLOADS_STORAGE) || '[]'); } catch { return []; }
}

/** Records one finished download for the history panel. */
function rememberDownload(entry) {
  // The live row was registered under the id the download command used, which may be the
  // original id while the finished record is keyed by appid.
  const id = entry.liveId || entry.appid || entry.username;
  const avatar = liveDownloads.get(id)?.avatar || entry.avatar || '';
  finishLiveDownload(id);
  const downloads = loadDownloads();
  const record = { at: Date.now(), avatar, ...entry };
  // Keep the caller's object addressable by `at`, or later edits (projectDir) would be
  // written to a record that no longer matches anything in storage.
  entry.at = record.at;
  downloads.unshift(record);
  localStorage.setItem(DOWNLOADS_STORAGE, JSON.stringify(downloads.slice(0, 50)));
  renderDownloadBadge();
  // A visible download center must flip from the live bar to the finished
  // entry the moment the download lands, not on the next open.
  if (!document.getElementById('download-center').hidden) renderDownloads();
}

/** Records a finished download and folds it into the stored history. */
function persistDownload(entry) {
  const rows = loadDownloads().map((row) =>
    (row.at === entry.at && (row.appid || row.username) === (entry.appid || entry.username) ? entry : row));
  localStorage.setItem(DOWNLOADS_STORAGE, JSON.stringify(rows));
}

/** Turns one download response into a history record. */
function downloadRecord(item, data) {
  const report = (data && data.report) || {};
  const packages = report.packages || [];
  // Only packages this run actually fetched: skipped resource bundles are reported
  // separately and must not inflate the record's totals.
  const attempted = packages.filter((pack) => !pack.skipped);
  return {
    name: item.nickName || report.appid || item.username || t('dl.untitled', '小程序'),
    appid: item.appid || report.appid || '',
    username: item.username || '',
    avatar: item.icon || item.iconUrl || '',
    outDir: report.outDir || '',
    projectDir: (data && data.projectDir) || null,
    packages: attempted.filter((pack) => !pack.error && pack.path).map((pack) => pack.path),
    failed: attempted.filter((pack) => pack.error).length,
    total: attempted.length,
    skipped: packages.length - attempted.length,
  };
}

/** Restores one history record; shared by the row action, the menu and the detail sheet. */
async function restoreDownload(entry, beautify = state.settings.beautify !== false) {
  if (entry.projectDir) {
    revealPath(entry.projectDir);
    return null;
  }
  const paths = (entry.packages || []).filter(Boolean);
  if (paths.length === 0) throw new Error(t('dl.noPaths', '记录里没有保存包路径'));
  const folder = entry.appid ? `unpack_${entry.appid}` : null;
  const data = await call('unpack_files', {
    paths,
    key: entry.appid || null,
    out: entry.outDir || null,
    folder,
    beautify,
  });
  entry.projectDir = data.projectDir;
  persistDownload(entry);
  renderDownloads();
  revealPath(data.projectDir);
  return data.projectDir;
}

/** Copies the containers of one history record out, untouched. */
async function rawExportDownload(entry) {
  const paths = (entry.packages || []).filter(Boolean);
  if (paths.length === 0) {
    notify('warn', t('dl.noPaths', '记录里没有保存包路径'), { title: t('toast.rawFailed', '原样导出失败') });
    return;
  }
  const base = entry.outDir ? entry.outDir.replace(/[\\/][^\\/]*$/, '') : null;
  try {
    const data = await call('raw_export', { paths, out: base ? `${base}/raw` : null });
    notify('success', `${data.files} → ${data.outDir}`, {
      title: t('toast.rawDone', '原样导出完成'),
      action: { label: t('dl.openFolder', '打开目录'), run: () => revealPath(data.outDir) },
    });
  } catch (error) {
    notify('error', describe(error), { title: t('toast.rawFailed', '原样导出失败') });
  }
}

/** Detail sheet for a download record: local data only, no network round trip. */
function showDownloadInfo(entry) {
  $('modal-detail').classList.add('is-info');
  const icon = $('detail-icon');
  if (entry.avatar) {
    icon.src = entry.avatar;
    icon.style.visibility = 'visible';
  } else {
    icon.style.visibility = 'hidden';
  }
  $('detail-id').textContent = entry.name || entry.appid || t('downloads.title', '下载记录');
  $('detail-sub').textContent = entry.appid || entry.username || '';
  const facts = $('detail-facts');
  facts.textContent = '';
  const rows = [
    ['APPID', entry.appid || '—'],
    [t('fact.origId', '原始 ID'), entry.username || '—'],
    [t('detail.downloadedAt', '下载时间'), entry.at ? clock(entry.at / 1000) : '—'],
    [t('detail.packages', '包数量'), `${(entry.packages || []).length}${entry.failed ? `（${tf('dl.failedCount', '{n} 个失败', { n: entry.failed })}）` : ''}`],
    [t('detail.packagePath', '包路径'), entry.outDir || '—'],
    [t('detail.projectPath', '工程路径'), entry.projectDir || t('detail.notRestored', '尚未还原')],
  ];
  for (const [name, value] of rows) {
    facts.append(el('dt', null, name));
    facts.append(el('dd', null, value));
  }
  $('detail-advanced').hidden = true;
  $('detail-structure').hidden = true;
  openModal('modal-detail');
}

/** Right-click menu for a download record, mirroring the local rows. */
function downloadMenu(event, entry) {
  const items = [
    { label: t('menu.detail', '查看详情'), action: () => showDownloadInfo(entry) },
    { label: t('menu.unpack', '还原源码'), action: () => restoreDownload(entry).catch((error) => notify('error', describe(error), { title: t('toast.unpackFailed', '还原源码失败') })) },
    { label: t('menu.raw', '原样导出'), action: () => rawExportDownload(entry) },
    '-',
    { label: t('menu.openLocation', '打开包路径'), action: () => revealPath(entry.outDir || entry.projectDir) },
    { label: t('btn.copyPath', '复制路径'), action: () => copyText(entry.outDir || entry.projectDir || '') },
    { label: t('menu.copyAppid', '复制 AppID'), action: () => copyText(entry.appid) },
  ];
  showMenu(event.clientX, event.clientY, items);
}

/** Deletes one record from the history; files on disk are left untouched. */
function removeDownload(entry) {
  const rows = loadDownloads().filter((row) => !(row.at === entry.at && (row.appid || row.username) === (entry.appid || entry.username)));
  localStorage.setItem(DOWNLOADS_STORAGE, JSON.stringify(rows));
  renderDownloadBadge();
  renderDownloads();
}

/** Draws the history panel from the stored records. */
function renderDownloads() {
  const list = $('download-list');
  list.textContent = '';
  for (const entry of liveDownloads.values()) list.append(liveDownloadNode(entry));
  const downloads = loadDownloads();
  if (downloads.length === 0 && liveDownloads.size === 0) {
    const empty = el('div', 'empty');
    empty.append(el('strong', null, t('dl.emptyTitle', '还没有下载记录')));
    empty.append(el('span', null, t('dl.emptyHint', '下载一个小程序后就会出现在这里。')));
    list.append(empty);
    return;
  }
  if (liveDownloads.size > 0 && downloads.length > 0) {
    list.append(el('div', 'hint', '—'));
  }
  for (const entry of downloads) {
    const item = el('div', 'download-item');
    item.title = t('dl.menuHint', '右键查看更多操作');

    const remove = el('button', 'dl-delete', '✕');
    remove.type = 'button';
    remove.title = t('dl.deleteRecord', '删除这条记录（不删除文件）');
    remove.addEventListener('click', (event) => {
      event.stopPropagation();
      removeDownload(entry);
    });

    const avatar = el('span', 'avatar');
    if (entry.avatar) {
      const img = el('img');
      img.src = entry.avatar;
      img.alt = '';
      img.referrerPolicy = 'no-referrer';
      avatar.append(img);
    } else {
      avatar.textContent = (entry.name || entry.appid || '·').slice(0, 1);
    }

    const info = el('div', 'dl-main');
    const head = el('div', 'dl-head');
    head.append(
      el('span', 'name', entry.name || entry.appid || entry.username || t('dl.untitled', '小程序')),
      el('span', 'dl-state', entry.projectDir ? t('dl.stateRestored', '已还原') : (entry.failed ? tf('dl.stateFailed', '{n} 个包失败', { n: entry.failed }) : t('dl.stateDownloaded', '已下载'))),
    );
    info.append(head);
    info.append(el('div', 'meta mono', `${entry.appid || entry.username || ''} · ${entry.at ? clock(entry.at / 1000) : '—'}`));
    const total = entry.total || (entry.packages || []).length || 1;
    const ok = Math.max(0, total - (entry.failed || 0));
    const bar = el('div', 'bar');
    const fill = el('div', 'bar-fill');
    fill.style.width = `${Math.round((ok / total) * 100)}%`;
    bar.append(fill);
    info.append(bar);

    const actions = el('div', 'dl-actions');
    const restore = el('button', 'btn small', entry.projectDir ? t('dl.openSource', '打开源码') : t('dl.restore', '还原源码'));
    restore.type = 'button';
    restore.title = t('dl.restoreHint', '按设置里的默认选项执行');
    restore.addEventListener('click', async (event) => {
      event.stopPropagation();
      restore.disabled = true;
      const before = restore.textContent;
      restore.textContent = t('btn.unpacking', '还原中…');
      try {
        await restoreDownload(entry, state.settings.beautify !== false);
      } catch (error) {
        notify('error', describe(error), { title: t('toast.unpackFailed', '还原源码失败') });
      } finally {
        restore.disabled = false;
        restore.textContent = before;
      }
    });
    const open = el('button', 'btn small quiet', t('dl.openFolder', '打开目录'));
    open.type = 'button';
    open.addEventListener('click', (event) => {
      event.stopPropagation();
      revealPath(entry.projectDir || entry.outDir);
    });
    actions.append(restore, open);
    item.append(remove, avatar, info, actions);
    item.addEventListener('click', () => showDownloadInfo(entry));
    item.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      event.stopPropagation();
      downloadMenu(event, entry);
    });
    list.append(item);
  }
}

/** Live per-program download progress: registered by name, fed by the progress event. */
const liveDownloads = new Map();

/** Registers a starting download under its id with a human name. */
function noteDownloadStart(id, name, avatar) {
  if (!id) return;
  liveDownloads.set(id, { id, name: name || id, avatar: avatar || '', done: 0, total: 0, module: '' });
  renderDownloadBadge();
  if (!document.getElementById('download-center').hidden) renderDownloads();
}

function noteDownloadProgress(id, done, total, module) {
  const entry = liveDownloads.get(id) || { id, name: id, done: 0, total, module: '' };
  entry.done = done; entry.total = total; entry.module = module;
  liveDownloads.set(id, entry);
  renderDownloadBadge();
  if (!document.getElementById('download-center').hidden) renderDownloads();
}

/** Marks a live entry finished and folds it into the stored history. */
function finishLiveDownload(id, entry) {
  liveDownloads.delete(id);
  renderDownloadBadge();
  if (!document.getElementById('download-center').hidden) renderDownloads();
}

function renderDownloadBadge() {
  const count = loadDownloads().length + liveDownloads.size;
  const badge = $('download-count');
  if (badge) {
    badge.hidden = count === 0;
    badge.textContent = count > 99 ? '99+' : String(count);
  }
}

/** One live entry with its own progress bar. */
function liveDownloadNode(entry) {
  const item = el('div', 'download-item');
  const spacer = el('span');
  const avatar = el('span', 'avatar');
  if (entry.avatar) {
    const img = el('img');
    img.src = entry.avatar;
    img.alt = '';
    img.referrerPolicy = 'no-referrer';
    avatar.append(img);
  } else {
    avatar.textContent = (entry.name || entry.id || '·').slice(0, 1);
  }
  const info = el('div', 'dl-main');
  const head = el('div', 'dl-head');
  const pct = entry.total ? Math.min(100, Math.round((entry.done * 100) / entry.total)) : 0;
  const state = el('span', 'dl-state mono');
  state.append(el('span', 'spin'));
  state.append(document.createTextNode(`${entry.done}/${entry.total} · ${pct}%`));
  head.append(el('span', 'name', entry.name || entry.id || t('dl.untitled', '小程序')), state);
  info.append(head);
  info.append(el('div', 'meta mono', entry.module || t('dl.writing', '写入中…')));
  const bar = el('div', 'bar');
  const fill = el('div', 'bar-fill');
  fill.style.width = `${pct}%`;
  bar.append(fill);
  info.append(bar);

  const actions = el('div', 'dl-actions');
  const cancel = el('button', 'dl-cancel', '✕');
  cancel.type = 'button';
  cancel.title = t('dl.cancel', '取消下载');
  cancel.disabled = Boolean(entry.cancelling);
  cancel.addEventListener('click', (event) => {
    event.stopPropagation();
    cancelDownload(entry.id);
  });
  actions.append(cancel);
  item.append(spacer, avatar, info, actions);
  return item;
}

/** Asks the backend to stop one running download. */
async function cancelDownload(id) {
  const entry = liveDownloads.get(id);
  if (!entry) return;
  entry.cancelling = true;
  if (!document.getElementById('download-center').hidden) renderDownloads();
  try {
    const accepted = await call('ilink_cancel_download', { id });
    if (!accepted) notify('warn', t('dl.cancelMissed', '这个下载已经结束'), { title: t('dl.cancel', '取消下载') });
  } catch (error) {
    notify('error', describe(error), { title: t('dl.cancel', '取消下载') });
  }
}

/* English overrides for the marked strings; the Chinese text lives in index.html and
 * stays the source of truth, so a missing key degrades to Chinese instead of breaking. */
const I18N_EN = {
  'tab.local': 'Local', 'tab.remote': 'Remote download', 'filter.ph': 'Filter appid / name',
  'sel.all': 'Select all', 'sel.invert': 'Invert', 'btn.exportSelected': 'Export selected',
  'btn.batch': 'Restore source…', 'btn.query': 'Look up', 'btn.download': 'Download',
  'btn.downloadSelected': 'Download selected', 'btn.refreshRecent': 'Refresh recent',
  'opt.unpack': 'Unpack after download',
  'opt.allPackages': 'Resource packages (large)',
  'dlg.title': 'Download options',
  'dlg.targets': '{n} Mini Programs',
  'dlg.hint': 'The record becomes actionable as soon as the download finishes.',
  'dlg.start': 'Start download',
  'dl.writing': 'Writing…',
  'dl.cancel': 'Cancel download',
  'dl.cancelMissed': 'this download has already finished',
  'dl.deleteRecord': 'Delete this record (files are kept)',
  'toast.downloadCancelled': 'Download cancelled',
  'toast.openFailed': 'Could not open the system browser', 'opt.beautify': 'Beautify', 'opt.verify': 'Verify',
  'settings.title': 'Settings', 'settings.export': 'Export', 'settings.output': 'Default output directory',
  'settings.defaults': 'Defaults', 'settings.appearance': 'Appearance',
  'theme.system': 'System', 'theme.light': 'Light', 'theme.dark': 'Dark',
  'settings.env': 'Environment', 'btn.openData': 'Open data directory', 'settings.cache': 'Cache',
  'btn.purge': 'Clear reconstructions', 'settings.api': 'API server', 'btn.start': 'Start',
  'btn.stop': 'Stop', 'btn.save': 'Save', 'btn.pick': 'Choose…', 'btn.clear': 'Clear',
  'drawer.logs': 'Logs', 'batch.title': 'Restore source', 'batch.run': 'Start',
  'downloads.title': 'Downloads',
  'col.name': 'Mini Program', 'col.version': 'Version', 'col.packages': 'Packages',
  'col.size': 'Size', 'col.updated': 'Updated', 'col.state': 'State',
  'col.rname': 'Name', 'col.rusername': 'Original ID', 'col.rappid': 'appid',
  'col.rused': 'Last used',
  'state.extracted': 'Restored', 'state.pending': 'Not restored',
  'menu.detail': 'Details', 'menu.unpack': 'Restore source', 'menu.raw': 'Export raw',
  'menu.openLocation': 'Open package folder', 'menu.openProject': 'Open project folder',
  'menu.copyAppid': 'Copy appid',
  'fact.location': 'Location', 'fact.version': 'Version', 'fact.packages': 'Packages',
  'fact.size': 'Size', 'fact.updated': 'Updated', 'fact.state': 'State', 'fact.project': 'Project',
  'fact.appid': 'appid', 'fact.origId': 'Original ID', 'fact.baseLib': 'Base library',
  'fact.files': 'Files',
  'toast.unpacked': 'Restore source complete', 'toast.unpackFailed': 'Restore source failed',
  'toast.rawDone': 'Raw export complete', 'toast.rawFailed': 'Raw export failed',
  'toast.downloadDone': 'Download complete',
  'toast.downloadCount': 'Download complete · {n} package(s)',
  'toast.partialCount': '{ok} package(s) succeeded, {failed} failed',
  'toast.skippedResource': '{n} resource package(s) skipped; tick "Resource packages" before downloading if needed', 'toast.downloadFailed': 'Download failed',
  'toast.saved': 'Settings saved', 'toast.saveFailed': 'Could not save settings',
  'toast.deleted': 'Account deleted', 'toast.deleteFailed': 'Could not delete the account',
  'toast.loginOk': 'Login successful', 'toast.needLogin': 'Please scan to sign in again',
  'toast.recentFailed': 'Could not read recent Mini Programs', 'toast.infoFailed': 'Lookup failed',
  'btn.unpack': 'Restore source', 'btn.unpacking': 'Restoring…', 'btn.raw': 'Export raw',
  'btn.inspect': 'Inspect', 'btn.inspecting': 'Reading…', 'btn.downloadRow': 'Download',
  'btn.downloading': 'Downloading', 'btn.downloaded': 'Downloaded',
  'acct.relogin': 'Scan again', 'acct.delete': 'Delete', 'acct.notSignedIn': 'Not signed in',
  'acct.online': 'Online', 'acct.stale': 'Sign-in expired', 'acct.offline': 'Offline',
  'empty.noRecent': 'No recent Mini Programs yet', 'empty.noRecentHint': 'Sign in by scanning, then read the recent list.',
  'empty.noMatch': 'No matches', 'empty.noMatchHint': 'Try another keyword, or clear the filter.',
  'qr.hint': 'Scan with WeChat, then confirm on your phone', 'qr.expired': 'The QR code expired; fetch a new one',
  'login.title': 'WeChat scan login', 'login.restart': 'Fetch again',
  'drawer.title': 'Logs',
  'about.title': 'About & feedback',
  'about.hint': 'Click the WeChat card to copy the id.',
  'acct.confirmDelete': 'Delete account {who}? Its icon and recent caches are removed too.',
  'acct.current': 'current account',
  'acct.loading': 'Reading the session…',
  'acct.signedIn': 'Account signed in',
  'acct.status': 'Account status',
  'acct.wechatUser': 'WeChat user',
  'batch.done': 'Batch export complete',
  'batch.failed': 'Batch export failed',
  'batch.none': 'No Mini Program selected.',
  'batch.ok': '{n} exported',
  'batch.partial': 'Batch export partly failed',
  'batch.result': '{ok} succeeded, {failed} failed',
  'batch.summary': '{n} selected; each goes into its own appid folder.',
  'batch.waiting': 'Waiting',
  'btn.advanced': 'Advanced…',
  'btn.close': 'Close',
  'btn.copyPath': 'Copy path',
  'btn.downloadingShort': 'Downloading',
  'btn.exporting': 'Exporting…',
  'btn.grant': 'Grant file access…',
  'btn.loading': 'Loading…',
  'btn.retry': 'Retry',
  'btn.reveal': 'Show folder',
  'btn.unpackShort': 'Restore',
  'cache.confirm': 'Delete the reconstructions this app created in the workspace?',
  'cache.facts': '{n} reconstruction(s) · {size}',
  'cache.failed': 'Cleanup failed',
  'cache.purged': 'Removed {n} folder(s), freed {size}',
  'decrypt.done': 'Folder decryption complete',
  'decrypt.failed': 'Decryption failed',
  'decrypt.folderPh': 'Folder containing wxapkg files',
  'decrypt.keysPh': 'One key per line; several are tried in turn',
  'decrypt.partial': 'Some files could not be decrypted',
  'decrypt.result': 'Decrypted {n}, plaintext {p}, failed {f}',
  'decrypt.run': 'Start decrypting',
  'decrypt.running': 'Decrypting…',
  'decrypt.title': 'Decrypt a folder',
  'detail.copyHint': 'Click to copy the path',
  'detail.downloadedAt': 'Downloaded',
  'detail.encrypted': 'V1MMWX (handled automatically)',
  'detail.encryption': 'Encryption',
  'detail.notRestored': 'Not restored yet',
  'detail.packagePath': 'Package folder',
  'detail.packages': 'Packages',
  'detail.plain': 'Not encrypted',
  'detail.projectPath': 'Project folder',
  'detail.queryFailed': 'Lookup failed',
  'detail.querying': 'Looking up…',
  'detail.status': 'Status',
  'detail.summary': 'v{v} · {n} package(s) · {size}',
  'detail.title': 'Mini Program',
  'dl.emptyHint': 'Downloaded Mini Programs show up here.',
  'dl.emptyTitle': 'No downloads yet',
  'dl.failedCount': '{n} failed',
  'dl.menuHint': 'Right-click for more actions',
  'dl.noPaths': 'this record has no saved package paths',
  'dl.openFolder': 'Open folder',
  'dl.openSource': 'Open source',
  'dl.restore': 'Restore source',
  'dl.restoreHint': 'Uses the defaults from Settings',
  'dl.stateDownloaded': 'Downloaded',
  'dl.stateFailed': '{n} package(s) failed',
  'dl.stateRestored': 'Restored',
  'dl.untitled': 'Mini Program',
  'env.available': 'available',
  'env.dataDir': 'Data folder',
  'env.notFound': 'not found',
  'env.notFoundShort': 'WeChat data not found',
  'env.output': 'Output folder',
  'env.remote': 'Remote engine',
  'env.remoteNative': 'built-in',
  'env.system': 'System',
  'env.unavailable': 'unavailable',
  'env.version': 'Version',
  'env.wechatData': 'WeChat data',
  'field.folder': 'Folder',
  'field.key': 'Decryption key',
  'field.keyPh': 'wx + 16 characters (leave empty if unencrypted)',
  'field.options': 'Options',
  'field.output': 'Output folder',
  'field.outputPh': 'Leave empty to use the default',
  'flag.keepRaw': 'Keep original packages',
  'flag.plugins': 'Clean plugins',
  'flag.prune': 'Trim empty pages',
  'flag.requires': 'Fix require()',
  'flag.synthesize': 'Fill missing pages',
  'flag.wxml': 'Restore WXML',
  'header.decrypt': 'Decrypt folder…',
  'header.downloads': 'Downloads',
  'header.logs': 'Logs',
  'header.rescan': 'Rescan',
  'header.settings': 'Settings',
  'header.unpack': 'Unpack file…',
  'hint.keyIsAppid': 'The key is the Mini Program appid: wx + 16 characters, e.g. wxabcdef1234567890.',
  'inspect.loading': 'Reading the container structure…',
  'job.running': 'Working',
  'login.fetching': 'Fetching the QR code…',
  'login.left': 'Time left',
  'login.noQr': 'the service returned no QR code',
  'login.ok': 'Login successful',
  'login.okNamed': 'Login successful: {name}',
  'login.persist': 'The session stays on this machine; scan again if it expires.',
  'login.pollFailed': 'Could not check the login: {err}',
  'login.qrFailed': 'Could not fetch the QR code',
  'login.qrImageFailed': 'The QR image failed to load; press "Fetch again"',
  'login.qrImageFailedShort': 'Could not load the QR image',
  'login.status': 'Login status: {status}',
  'perm.missing': 'This system has no such settings page',
  'perm.opened': 'Opened the Full Disk Access settings',
  'pick.failed': 'Could not choose a folder',
  'pick.folder': 'Choose a folder containing wxapkg files',
  'pick.needFolder': 'Choose a folder first',
  'pick.needWxapkg': 'Choose a wxapkg file first',
  'pick.output': 'Choose the output folder',
  'pick.wxapkg': 'Choose a wxapkg file',
  'qr.confirm': 'Scan with WeChat, then confirm on your phone',
  'qr.invalid': 'This QR code is no longer valid; fetch a new one',
  'qr.scanned': 'Scanned — confirm on your phone',
  'query.invalid': 'Enter an appid or an original ID',
  'recent.failed': 'Could not read the list',
  'remote.notReady': 'the remote engine is not ready',
  'remote.queryPh': 'Appid (wx…) or original ID (gh_…@app)',
  'remote.unavailable': 'Remote download unavailable',
  'scan.denied': '{n} folders were denied; the list may be incomplete. File access can be granted in Settings.',
  'scan.failed': 'Scan failed',
  'scan.none': 'No Mini Programs found',
  'scan.noneHint': 'Make sure WeChat has opened a Mini Program on this machine, or check file access in Settings.',
  'scan.skipped': '{n} paths could not be read; the list may be incomplete',
  'scan.stale': 'Showing the previous scan ({time}); refreshing…',
  'sel.allRecent': 'Select all recent',
  'sel.count': '{n} Mini Program(s)',
  'sel.one': 'Select {id}',
  'sel.summary': '{n} of {total} selected',
  'settings.apiHint': 'Independent of the window; open /docs for the API reference.',
  'settings.concurrency': 'Parallel downloads',
  'settings.concurrencyHint': '1–10, default 3',
  'settings.keyPh': 'Access key (optional)',
  'settings.language': 'Language',
  'settings.listen': 'Listen',
  'settings.savedHint': 'Settings are stored locally and apply immediately.',
  'state.failed': 'Failed',
  'status.denied': 'File access denied',
  'status.remote': 'Remote {impl}',
  'status.remoteDown': 'Remote unavailable',
  'svc.badPort': 'Invalid port',
  'svc.running': 'Running · http://{url} · docs /docs',
  'svc.startFailed': 'Could not start the API service',
  'svc.started': 'API service started',
  'svc.stopped': 'Not running',
  'svc.stoppedToast': 'API service stopped',
  'toast.batchDone': 'Batch download complete',
  'toast.batchPartial': 'Batch download partly failed',
  'toast.copied': 'Copied to clipboard',
  'unpack.file': 'Container file',
  'unpack.filePh': 'Choose or paste a .wxapkg path',
  'unpack.keyRequired': 'Decryption key required',
  'unpack.needKey': 'This container is encrypted; enter its key (the Mini Program appid)',
  'unpack.run': 'Start unpacking',
  'unpack.multi': 'Restored {n} Mini Programs, one folder per appid',
  'unpack.running': 'Unpacking…',
  'unpack.title': 'Unpack a specific wxapkg',
  'welcome.start': 'Get started',
  'toast.downloadStarted': 'Download started',
  'relative.now': 'just now',
  'relative.minutes': '{n} min ago',
  'relative.hours': '{n} h ago',
  'relative.days': '{n} d ago',
  'toast.title.success': 'Success',
  'toast.title.error': 'Error',
  'toast.title.warn': 'Notice',
  'toast.title.info': 'Info',
  'job.done': 'Done',
  'job.failed': 'Failed',
  'filter.localPh': 'Filter appid / version',
  'filter.remotePh': 'Filter name / original ID',
  'toast.startUnpack': 'Starting the restore',
  'decrypt.hint': 'The key is the Mini Program appid: wx + 16 characters, e.g. wxabcdef1234567890. Every wxapkg in the folder is tried against all keys; plaintext files are copied as-is.',
  'qr.alt': 'Login QR code',
  'recent.count': '{n} record(s)',
  'recent.none': '0 records',
  'recent.copyUsername': 'Copy original ID',
  'welcome.sub': 'Restore Mini Program sources from your local cache or a remote download, all in one place.',
  'welcome.title': 'Welcome to Wxapkg Dump',
};
const I18N_ZH_EXTRA = { 'sel.invert': '反选' };
const zhDefaults = new Map();

/** Chinese for Chinese-language systems, English everywhere else, unless the user chose. */
function detectLang() {
  const saved = localStorage.getItem(LANG_STORAGE);
  if (saved === 'zh' || saved === 'en') return saved;
  const tags = (navigator.languages && navigator.languages.length)
    ? navigator.languages
    : [navigator.language || ''];
  return tags.some((tag) => /^zh([-_]|$)/i.test(tag)) ? 'zh' : 'en';
}

function currentLang() {
  const chosen = state.settings?.lang;
  if (chosen === 'zh' || chosen === 'en') return chosen;
  return detectLang();
}

function t(key, zh) {
  if (currentLang() === 'en') return I18N_EN[key] || zh;
  return I18N_ZH_EXTRA[key] || zh;
}

/** Same as `t`, with `{name}` placeholders filled in. */
function tf(key, zh, vars) {
  let text = t(key, zh);
  for (const [name, value] of Object.entries(vars || {})) {
    text = text.split(`{${name}}`).join(String(value));
  }
  return text;
}

function applyLang(lang) {
  localStorage.setItem(LANG_STORAGE, lang);
  document.documentElement.lang = lang === 'en' ? 'en' : 'zh-CN';
  for (const node of document.querySelectorAll('[data-i18n]')) {
    const key = node.dataset.i18n;
    if (lang === 'en') node.textContent = I18N_EN[key] || zhDefaults.get(key) || node.textContent;
    else node.textContent = zhDefaults.get(key) || node.textContent;
  }
  for (const node of document.querySelectorAll('[data-i18n-ph]')) {
    const key = node.dataset.i18nPh;
    if (lang === 'en') node.placeholder = I18N_EN[key] || node.placeholder;
    else node.placeholder = zhDefaults.get(key + '.ph') || node.placeholder;
  }
  for (const node of document.querySelectorAll('[data-i18n-title]')) {
    const key = node.dataset.i18nTitle;
    const text = lang === 'en' ? (I18N_EN[key] || node.title) : (I18N_ZH_EXTRA[key] || node.title);
    node.title = text;
    if (node.hasAttribute('aria-label')) node.setAttribute('aria-label', text);
  }
  for (const node of document.querySelectorAll('[data-i18n-alt]')) {
    const key = node.dataset.i18nAlt;
    node.alt = lang === 'en' ? (I18N_EN[key] || node.alt) : (I18N_ZH_EXTRA[key] || node.alt);
  }
  for (const node of document.querySelectorAll('[data-i18n-aria]')) {
    const key = node.dataset.i18nAria;
    const text = lang === 'en' ? (I18N_EN[key] || node.getAttribute('aria-label')) : (I18N_ZH_EXTRA[key] || node.getAttribute('aria-label'));
    node.setAttribute('aria-label', text);
  }
  for (const node of document.querySelectorAll('[data-i18n-col]')) {
    const key = node.dataset.i18nCol;
    const text = lang === 'en' ? (I18N_EN[key] || zhDefaults.get(key)) : zhDefaults.get(key);
    if (text && node.firstChild) node.firstChild.textContent = text;
  }
  for (const seg of document.querySelectorAll('#lang-group .seg')) {
    seg.classList.toggle('is-active', seg.dataset.lang === lang);
  }
  $('filter').placeholder = lang === 'en'
    ? (I18N_EN['filter.ph'] || zhDefaults.get('filter.ph') || $('filter').placeholder)
    : (zhDefaults.get('filter.ph') || $('filter').placeholder);
  if (state.programs.length || state.recent.length) {
    if (state.view === 'local') renderPrograms(); else renderRecent();
  }
}

const COLUMNS = {
  appid: { label: '小程序', min: 140, default: 230 },
  version: { label: '版本', min: 60, default: 84 },
  packages: { label: '分包', min: 52, default: 64 },
  size: { label: '大小', min: 64, default: 92 },
  updated: { label: '修改时间', min: 150, default: 180 },
  'r-name': { label: '名称', min: 140, default: null },
  'r-username': { label: '原始 ID', min: 120, default: 210 },
  'r-appid': { label: 'appid', min: 120, default: 190 },
  'r-used': { label: '最近使用', min: 110, default: 150 },
};

const state = {
  view: 'local',
  programs: [],
  selected: new Set(),
  settings: {
    defaultOutput: '',
    beautify: true,
    verify: true,
    theme: 'system',
    columns: {},
  },
  env: null,
  accounts: [],
  account: null,
  sort: { key: 'updated', desc: true },
  recent: [],
  recentSelected: new Set(),
  recentLoaded: false,
  recentLoading: false,
  login: null,
  job: null,
  iconCache: new Map(),
  iconQueue: [],
  iconActive: 0,
  recentIcons: new Map(),
  recentRows: new Map(),
  recentIconQueue: [],
  recentIconActive: 0,
  scanSavedAt: null,
  animateRows: true,
  logCount: 0,
  drawer: false,
};

/* ---------------------------------------------------------------- helpers */

const $ = (id) => document.getElementById(id);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function bytes(value) {
  const size = Number(value) || 0;
  const units = ['B', 'KB', 'MB', 'GB'];
  let index = 0;
  let rest = size;
  while (rest >= 1024 && index < units.length - 1) {
    rest /= 1024;
    index += 1;
  }
  return `${rest.toFixed(size >= 100 || index === 0 ? 0 : 1)} ${units[index]}`;
}

function clock(value, withTime = true) {
  if (!value) return '—';
  const date = new Date(value * (value < 1e12 ? 1000 : 1));
  if (Number.isNaN(date.getTime())) return '—';
  const pad = (n) => String(n).padStart(2, '0');
  const day = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
  return withTime
    ? `${day} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
    : day;
}

function relative(value) {
  if (!value) return '';
  const seconds = Math.max(0, Math.round(Date.now() / 1000 - value));
  if (seconds < 60) return t('relative.now', '刚刚');
  if (seconds < 3600) return tf('relative.minutes', '{n} 分钟前', { n: Math.round(seconds / 60) });
  if (seconds < 86400) return tf('relative.hours', '{n} 小时前', { n: Math.round(seconds / 3600) });
  if (seconds < 86400 * 30) return tf('relative.days', '{n} 天前', { n: Math.round(seconds / 86400) });
  return '';
}

function basename(path) {
  if (!path) return '';
  return String(path).split(/[\\/]/).filter(Boolean).pop() || '';
}

function seconds(value) {
  const total = Math.max(0, Math.round(value));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`;
}

function osLabel(os, arch) {
  const names = { macos: 'macOS', windows: 'Windows', linux: 'Linux' };
  return `${names[os] || os} ${arch}`;
}

function describe(error) {
  if (typeof error === 'string') return error;
  if (error && typeof error.message === 'string') return error.message;
  return String(error);
}

function key(appid, version) {
  return `${appid}@${version || ''}`;
}

/* ----------------------------------------------------------------- toasts */

const TOAST_GLYPH = { success: '✓', error: '✕', warn: '!', info: 'i' };
const TOAST_TITLE = {
  success: () => t('toast.title.success', '成功'),
  error: () => t('toast.title.error', '失败'),
  warn: () => t('toast.title.warn', '注意'),
  info: () => t('toast.title.info', '提示'),
};

/**
 * Shows a result message. `level` is one of success / error / warn / info; an optional
 * action renders as a link inside the card.
 */
function notify(level, message, options = {}) {
  const stack = $('toasts');
  const kind = TOAST_GLYPH[level] ? level : 'info';
  const card = el('div', `toast ${kind}`);
  card.setAttribute('role', level === 'error' ? 'alert' : 'status');

  const glyph = el('span', 'glyph', TOAST_GLYPH[kind]);
  const body = el('div', 'body');
  body.append(el('div', 'title', options.title || TOAST_TITLE[kind]()));
  body.append(el('div', 'detail', message));

  if (options.action) {
    const action = el('button', 'action', options.action.label);
    action.type = 'button';
    action.addEventListener('click', () => {
      dismiss();
      options.action.run();
    });
    body.append(action);
  }

  card.append(glyph, body);
  card.addEventListener('click', (event) => {
    if (event.target === card) dismiss();
  });
  stack.append(card);

  while (stack.children.length > 4) stack.firstElementChild.remove();

  const lifetime = options.timeout || (kind === 'error' ? 9000 : kind === 'warn' ? 7000 : 4200);
  let timer = setTimeout(dismiss, lifetime);

  function dismiss() {
    clearTimeout(timer);
    card.classList.add('is-leaving');
    setTimeout(() => card.remove(), 160);
  }

  // Hovering pauses the countdown: the reader decides when it goes away.
  card.addEventListener('mouseenter', () => clearTimeout(timer));
  card.addEventListener('mouseleave', () => {
    timer = setTimeout(dismiss, 2200);
  });

  return card;
}

/* ------------------------------------------------------------------- log */

function addLog(kind, line) {
  const list = $('log-list');
  const item = el('li', `log-line ${kind || 'info'}`);
  item.append(el('span', 'log-time', new Date().toLocaleTimeString('zh-CN', { hour12: false })));
  item.append(el('span', 'log-text', line));
  list.append(item);
  while (list.children.length > 500) list.removeChild(list.firstChild);
  list.scrollTop = list.scrollHeight;
  state.logCount += 1;
  if (!state.drawer) $('log-dot').hidden = false;
}

/* ----------------------------------------------------------------- theme */

function applyTheme(mode) {
  const root = document.documentElement;
  if (mode === 'system') root.removeAttribute('data-theme');
  else root.dataset.theme = mode;
  for (const seg of document.querySelectorAll('#theme-group .seg')) {
    seg.classList.toggle('is-active', seg.dataset.theme === mode);
  }
  localStorage.setItem(THEME_STORAGE, mode);
}

/* --------------------------------------------------------------- columns */

/** Applies stored widths to both tables; missing ones keep their default. */
function applyColumns() {
  for (const table of document.querySelectorAll('.grid')) {
    for (const col of table.querySelectorAll('col[data-col]')) {
      const name = col.dataset.col;
      const width = state.settings.columns?.[name];
      if (width) col.style.width = `${width}px`;
      else if (COLUMNS[name]?.default) col.style.width = `${COLUMNS[name].default}px`;
      else col.style.width = '';
    }
  }
}

function setupColumnResize() {
  let drag = null;

  document.addEventListener('mousedown', (event) => {
    const grip = event.target.closest('.grip');
    if (!grip) return;
    const name = grip.dataset.grip;
    const table = grip.closest('table');
    const col = table.querySelector(`col[data-col="${name}"]`);
    const th = grip.closest('th');
    if (!col || !th) return;

    event.preventDefault();
    drag = { name, col, startX: event.clientX, startWidth: th.getBoundingClientRect().width };
    document.body.classList.add('is-resizing');
  });

  document.addEventListener('mousemove', (event) => {
    if (!drag) return;
    const limit = COLUMNS[drag.name]?.min || 60;
    const width = Math.max(limit, Math.round(drag.startWidth + event.clientX - drag.startX));
    drag.col.style.width = `${width}px`;
  });

  document.addEventListener('mouseup', () => {
    if (!drag) return;
    const width = Number.parseInt(drag.col.style.width, 10);
    state.settings.columns = { ...state.settings.columns, [drag.name]: width };
    drag = null;
    document.body.classList.remove('is-resizing');
    saveSettings({ silent: true });
  });

  // Double-click restores the designed width for that column.
  document.addEventListener('dblclick', (event) => {
    const grip = event.target.closest('.grip');
    if (!grip) return;
    const name = grip.dataset.grip;
    const table = grip.closest('table');
    const col = table.querySelector(`col[data-col="${name}"]`);
    if (!col) return;
    delete state.settings.columns[name];
    col.style.width = COLUMNS[name]?.default ? `${COLUMNS[name].default}px` : '';
    saveSettings({ silent: true });
  });
}

/* ------------------------------------------------------------------ jobs */

function jobStart(label) {
  stopJobTimer();
  state.job = { label, startedAt: Date.now(), timer: 0, phase: 'start' };
  $('jobbar').hidden = false;
  $('jobbar').classList.add('unknown');
  $('job-label').textContent = label;
  $('job-count').textContent = '—';
  $('job-msg').textContent = '';
  $('job-fill').style.width = '0%';
  $('job-time').textContent = '0.0s';
  state.job.timer = setInterval(() => {
    if (!state.job) return;
    $('job-time').textContent = `${((Date.now() - state.job.startedAt) / 1000).toFixed(1)}s`;
  }, 100);
}

function stopJobTimer() {
  if (state.job && state.job.timer) clearInterval(state.job.timer);
}

function jobUpdate(event) {
  if (event.phase === 'start') {
    jobStart(event.label);
    return;
  }
  if (!state.job) jobStart(event.label);
  if (event.total && event.done !== undefined && event.done !== null) {
    const ratio = Math.min(1, event.done / Math.max(1, event.total));
    $('jobbar').classList.remove('unknown');
    $('job-fill').style.width = `${Math.round(ratio * 100)}%`;
    $('job-count').textContent = `${event.done}/${event.total}`;
  }
  if (event.message) $('job-msg').textContent = event.message;
  if (event.phase === 'done' || event.phase === 'failed') {
    stopJobTimer();
    $('jobbar').classList.remove('unknown');
    if (event.phase === 'done') $('job-fill').style.width = '100%';
    const finished = state.job;
    const elapsed = finished ? (Date.now() - finished.startedAt) / 1000 : 0;
    $('job-time').textContent = `${elapsed.toFixed(1)}s`;
    $('job-label').textContent = event.phase === 'done' ? t('job.done', '完成') : t('job.failed', '失败');
    setTimeout(() => {
      if (state.job !== finished) return;
      $('jobbar').hidden = true;
      state.job = null;
    }, 1500);
  }
}

/* ----------------------------------------------------------------- views */

function showView(view) {
  state.view = view;
  for (const tab of document.querySelectorAll('.tab')) {
    const active = tab.dataset.view === view;
    tab.classList.toggle('is-active', active);
    tab.setAttribute('aria-selected', String(active));
  }
  for (const section of document.querySelectorAll('.view')) {
    section.classList.toggle('is-active', section.id === `view-${view}`);
  }
  $('filter').placeholder = view === 'local' ? t('filter.localPh', '过滤 appid / 版本') : t('filter.remotePh', '过滤名称 / 原始 ID');
  if (view === 'remote') void ensureRecent();
}

/// Loads the account when needed, then fetches the recent list once per login.
async function ensureRecent() {
  if (!state.account) await loadAccounts();
  if (state.account && !state.recentLoaded && !state.recentLoading) await loadRecent();
}

/* ----------------------------------------------------------------- modal */

function openModal(id) {
  hideMenu();
  $(id).hidden = false;
  const sheet = $(id).querySelector('.sheet');
  if (sheet) sheet.focus({ preventScroll: true });
}

function closeModal(id) {
  $(id).hidden = true;
  if (id === 'modal-login') stopLoginPolling();
}

function wireModals() {
  for (const modal of document.querySelectorAll('.modal')) {
    modal.addEventListener('click', (event) => {
      if (event.target === modal) closeModal(modal.id);
    });
    for (const button of modal.querySelectorAll('[data-close]')) {
      button.addEventListener('click', () => closeModal(modal.id));
    }
  }
  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    for (const modal of document.querySelectorAll('.modal')) {
      if (!modal.hidden) {
        closeModal(modal.id);
        return;
      }
    }
    if (state.drawer) toggleDrawer(false);
  });
}

/* ----------------------------------------------------------------- icons */

/** Icons come from WeChat's cache; a few at a time keeps the list responsive. */
function loadIcon(appid) {
  if (state.iconCache.has(appid)) return Promise.resolve(state.iconCache.get(appid));
  return new Promise((resolve) => {
    state.iconQueue.push({ appid, resolve });
    pumpIcons();
  });
}

function pumpIcons() {
  const LIMIT = 6;
  while (state.iconActive < LIMIT && state.iconQueue.length > 0) {
    const job = state.iconQueue.shift();
    state.iconActive += 1;
    call('program_icon', { appid: job.appid })
      .then((src) => src || null)
      .catch(() => null)
      .then((src) => {
        state.iconCache.set(job.appid, src);
        job.resolve(src);
      })
      .finally(() => {
        state.iconActive -= 1;
        pumpIcons();
      });
  }
}

/** Icons for remote rows, fetched only for the rows we actually render. */
function loadRecentIcon(username) {
  if (!username) return Promise.resolve(null);
  if (state.recentIcons.has(username)) return Promise.resolve(state.recentIcons.get(username));
  return new Promise((resolve) => {
    state.recentIconQueue.push({ username, resolve });
    pumpRecentIcons();
  });
}

function pumpRecentIcons() {
  const LIMIT = 4;
  while (state.recentIconActive < LIMIT && state.recentIconQueue.length > 0) {
    const job = state.recentIconQueue.shift();
    state.recentIconActive += 1;
    call('ilink_icon', { username: job.username })
      .then((src) => src || null)
      .catch(() => null)
      .then((src) => {
        state.recentIcons.set(job.username, src);
        job.resolve(src);
      })
      .finally(() => {
        state.recentIconActive -= 1;
        pumpRecentIcons();
      });
  }
}

function iconNode(src) {
  if (src) {
    const img = el('img', 'app-icon');
    img.alt = '';
    img.width = 20;
    img.height = 20;
    img.src = src;
    return img;
  }
  return el('span', 'app-icon');
}

/* ------------------------------------------------------------ context menu */

function showMenu(x, y, items) {
  const menu = $('menu');
  menu.textContent = '';
  for (const item of items) {
    if (item === '-') {
      menu.append(el('hr'));
      continue;
    }
    const button = el('button', item.danger ? 'danger' : null, item.label);
    button.type = 'button';
    button.addEventListener('click', () => {
      hideMenu();
      item.action();
    });
    menu.append(button);
  }
  menu.hidden = false;
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - rect.width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - rect.height - 8))}px`;

  const close = () => hideMenu();
  const onKey = (event) => {
    if (event.key === 'Escape') close();
  };
  setTimeout(() => {
    document.addEventListener('click', close, { once: true });
    document.addEventListener('keydown', onKey, { once: true });
    window.addEventListener('scroll', close, { once: true, capture: true });
  }, 0);
}

function hideMenu() {
  $('menu').hidden = true;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    notify('success', t('toast.copied', '已复制到剪贴板'));
    return;
  } catch {
    // Fall through to the selection-based copy below.
  }
  const area = el('textarea');
  area.value = text;
  area.style.position = 'fixed';
  area.style.opacity = '0';
  document.body.append(area);
  area.select();
  try {
    document.execCommand('copy');
    notify('success', t('toast.copied', '已复制到剪贴板'));
  } catch {
    notify('warn', text);
  }
  area.remove();
}

function revealPath(path) {
  if (!path) return;
  call('reveal', { path }).catch((error) => notify('error', describe(error)));
}

function rowMenu(event, program) {
  const items = [
    { label: t('menu.detail', '查看详情'), action: () => openDetail(program) },
    { label: t('menu.unpack', '还原源码'), action: () => unpackProgram(program) },
    { label: t('menu.raw', '原样导出'), action: () => rawExportProgram(program) },
    '-',
    { label: t('menu.openLocation', '打开包所在目录'), action: () => revealPath(program.location) },
  ];
  if (program.extracted && program.projectDir) {
    items.push({ label: t('menu.openProject', '打开工程目录'), action: () => revealPath(program.projectDir) });
  }
  items.push(
    { label: t('btn.copyPath', '复制路径'), action: () => copyText(program.location) },
    { label: t('menu.copyAppid', '复制 appid'), action: () => copyText(program.appid) },
  );
  showMenu(event.clientX, event.clientY, items);
}

/** Reconstructs one program straight from the row, using the global defaults. */
async function unpackProgram(program) {
  notify('info', program.appid, { title: t('toast.startUnpack', '开始还原') });
  try {
    const data = await call('unpack', {
      args: {
        appid: program.appid,
        paths: program.packagePaths || [],
        out: null,
        beautify: state.settings.beautify !== false,
        verify: state.settings.verify !== false,
        options: {},
      },
    });
    program.extracted = true;
    program.projectDir = data.projectDir;
    renderPrograms();
    notify('success', data.projectDir, {
      title: t('toast.unpacked', '还原源码完成'),
      action: { label: t('menu.openProject', '打开目录'), run: () => revealPath(data.projectDir) },
    });
  } catch (error) {
    notify('error', describe(error), { title: t('toast.unpackFailed', '还原源码失败') });
  }
}

/** Copies one program's containers out, untouched. */
async function rawExportProgram(program) {
  try {
    const data = await call('raw_export', {
      paths: program.packagePaths,
      out: null,
    });
    notify('success', `${data.files} → ${data.outDir}`, {
      title: t('toast.rawDone', '原样导出完成'),
      action: { label: t('menu.openProject', '打开目录'), run: () => revealPath(data.outDir) },
    });
  } catch (error) {
    notify('error', describe(error), { title: t('toast.rawFailed', '原样导出失败') });
  }
}

/* ----------------------------------------------------------------- local */

function skeletonRows(body, columns, count) {
  body.textContent = '';
  for (let i = 0; i < count; i += 1) {
    const row = el('tr', 'skeleton');
    if (body.id === 'programs-body' && i === 0) {
      row.title = '正在扫描本机小程序，稍候片刻…';
    }
    for (let c = 0; c < columns; c += 1) {
      const cell = el('td');
      const bar = el('div', 'bar');
      bar.style.width = `${30 + ((i * 7 + c * 13) % 55)}%`;
      cell.append(bar);
      row.append(cell);
    }
    body.append(row);
  }
}

function renderEmpty(container, title, hintLine) {
  container.textContent = '';
  container.append(el('strong', null, title));
  container.append(el('span', null, hintLine));
  container.hidden = false;
}

/** Reflects the sort state on the modified-time header. */
function syncSortHeader() {
  const th = $('th-updated');
  if (!th) return;
  th.classList.toggle('sorted-desc', state.sort?.key === 'updated' && state.sort.desc);
  th.classList.toggle('sorted-asc', state.sort?.key === 'updated' && !state.sort.desc);
}

function visiblePrograms() {
  const needle = $('filter').value.trim().toLowerCase();
  let rows = needle
    ? state.programs.filter((p) => p.appid.toLowerCase().includes(needle)
      || (p.version || '').toLowerCase().includes(needle))
    : [...state.programs];
  if (state.sort?.key === 'updated') {
    const dir = state.sort.desc ? -1 : 1;
    rows.sort((a, b) => dir * ((a.updatedAt || 0) - (b.updatedAt || 0)));
  }
  return rows;
}

function renderPrograms() {
  const body = $('programs-body');
  const empty = $('local-empty');
  body.textContent = '';
  const rows = visiblePrograms();
  empty.hidden = rows.length > 0;

  if (rows.length === 0) {
    if (state.programs.length === 0) {
      renderEmpty(empty, t('scan.none', '没有找到小程序'), t('scan.noneHint', '确认微信在本机登录过小程序，或检查设置里的文件访问权限。'));
    } else {
      renderEmpty(empty, t('empty.noMatch', '没有匹配的结果'), t('empty.noMatchHint', '换个关键字，或清空搜索框。'));
    }
  }

  const fragment = document.createDocumentFragment();
  rows.forEach((program, index) => {
    const id = key(program.appid, program.version);
    const row = el('tr', 'row');
    row.dataset.appid = program.appid;
    row.dataset.key = id;
    if (state.selected.has(id)) row.classList.add('is-selected');
    if (state.animateRows && index < 24) {
      row.classList.add('enter');
      row.style.animationDelay = `${Math.min(index, 20) * 9}ms`;
    }

    const checkCell = el('td', 'c-check');
    const check = el('input');
    check.type = 'checkbox';
    check.checked = state.selected.has(id);
    check.setAttribute('aria-label', tf('sel.one', '选择 {id}', { id: program.appid }));
    check.addEventListener('click', (event) => event.stopPropagation());
    check.addEventListener('change', () => {
      if (check.checked) state.selected.add(id);
      else state.selected.delete(id);
      row.classList.toggle('is-selected', check.checked);
      syncSelection();
    });
    checkCell.append(check);

    const stateCell = el('td', 'c-state');
    stateCell.append(el('span', `state${program.extracted ? ' done' : ''}`,
      program.extracted ? t('state.extracted', '已还原') : '—'));

    const updated = el('td', 'c-num mono', clock(program.updatedAt));

    row.append(
      checkCell,
      el('td', 'mono', program.appid),
      el('td', 'mono', program.version || '—'),
      el('td', 'c-num mono', String(program.packages || 0)),
      el('td', 'c-num mono', bytes(program.totalSize)),
      updated,
      stateCell,
    );

    row.addEventListener('click', () => openDetail(program));
    row.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      rowMenu(event, program);
    });
    fragment.append(row);
  });
  body.append(fragment);
  state.animateRows = false;
  syncSelection();
}

function selectableIds() {
  return visiblePrograms().map((p) => key(p.appid, p.version));
}

function syncSelection() {
  const all = selectableIds();
  const chosen = all.filter((id) => state.selected.has(id));
  $('selection').textContent = chosen.length
    ? tf('sel.summary', '已选 {n} / {total}', { n: chosen.length, total: state.programs.length })
    : `${state.programs.length} 个小程序`;
  $('btn-export-selected').disabled = chosen.length === 0;
  $('btn-batch').disabled = chosen.length === 0;
  $('select-all').checked = all.length > 0 && chosen.length === all.length;
  $('select-all').indeterminate = chosen.length > 0 && chosen.length < all.length;
}

function applyPrograms(rows, note) {
  state.programs = rows;
  state.selected.clear();
  state.animateRows = true;
  renderPrograms();
  const banner = $('local-note');
  if (banner) {
    banner.hidden = !note;
    banner.className = 'note';
    banner.textContent = note || '';
  }
}

/** Paints the previous scan immediately, then walks again in the background. */
async function bootPrograms() {
  try {
    const cache = await call('cached_scan');
    if (cache && cache.rows?.length) {
      state.scanSavedAt = cache.savedAt;
      applyPrograms(cache.rows, tf('scan.stale', '显示上次扫描结果（{time}），正在刷新…', { time: clock(cache.savedAt) }));
    } else {
      skeletonRows($('programs-body'), 8, 8);
    }
  } catch {
    skeletonRows($('programs-body'), 8, 8);
  }
  await loadPrograms(true);
}

async function loadPrograms(showJob) {
  const button = $('btn-refresh');
  button.disabled = true;
  button.classList.add('spin');
  if (showJob && $('local-note')) $('local-note').hidden = true;

  try {
    const view = await call('scan_programs');
    state.programs = view.programs || [];
    state.selected.clear();
    renderPrograms();

    const note = $('local-note');
    if (note && view.deniedPaths > 0) {
      note.hidden = false;
      note.className = 'note warn';
      note.textContent = tf('scan.denied', '{n} 个目录被系统拒绝，列表可能不完整；设置里可打开文件访问权限。', { n: view.deniedPaths });
      $('status-permission').hidden = false;
      $('status-permission').className = 'chip bad';
      $('status-permission').textContent = t('status.denied', '文件访问被拒绝');
    } else if (note) {
      note.hidden = state.programs.length > 0;
      if (state.programs.length === 0) {
        note.className = 'note warn';
        note.textContent = t('scan.noneHint', '没有找到小程序。确认微信已在本机登录过小程序。');
      }
      $('status-permission').hidden = true;
    }
    if (view.skippedPaths > 0) {
      addLog('warn', tf('scan.skipped', '{n} 个路径读取失败，列表可能不完整', { n: view.skippedPaths }));
    }
    $('selection').textContent = tf('sel.count', '{n} 个小程序', { n: state.programs.length });
  } catch (error) {
    const empty = $('local-empty');
    renderEmpty(empty, t('scan.failed', '扫描失败'), describe(error));
    empty.hidden = false;
    $('programs-body').textContent = '';
    notify('error', describe(error), { title: t('scan.failed', '扫描失败') });
  } finally {
    button.disabled = false;
    button.classList.remove('spin');
  }
}

/* ---------------------------------------------------------------- detail */

let detailProgram = null;

async function openDetail(program) {
  detailProgram = program;
  $('modal-detail').classList.remove('is-info');
  // The info sheet and the local sheet share #modal-detail; restore the local footer and
  // drop anything the remote view appended before filling it.
  const infoFoot = document.querySelector('#detail-info-foot');
  if (infoFoot) infoFoot.remove();
  const infoJson = document.querySelector('#info-json');
  if (infoJson) infoJson.remove();
  const localFoot = $('detail-local-foot');
  if (localFoot) localFoot.hidden = false;
  $('detail-id').textContent = program.appid;
  $('detail-sub').textContent =
    tf('detail.summary', 'v{v} · {n} 个包 · {size}', { v: program.version || '?', n: program.packages, size: bytes(program.totalSize) });

  const icon = $('detail-icon');
  icon.style.visibility = 'hidden';
  loadIcon(program.appid).then((src) => {
    if (!src) return;
    icon.src = src;
    icon.style.visibility = 'visible';
  });

  const facts = $('detail-facts');
  facts.textContent = '';
  const rows = [
    [t('fact.location', '位置'), program.location],
    [t('fact.updated', '最近更新'), `${clock(program.updatedAt)}${relative(program.updatedAt) ? ` · ${relative(program.updatedAt)}` : ''}`],
    [t('detail.encryption', '解密'), program.encrypted ? t('detail.encrypted', 'V1MMWX（已自动处理）') : t('detail.plain', '未加密')],
    [t('fact.state', '状态'), program.extracted ? t('state.extracted', '已还原') : t('state.pending', '未还原')],
  ];
  if (program.projectDir && program.extracted) rows.push([t('fact.project', '工程'), program.projectDir]);
  for (const [name, value] of rows) {
    facts.append(el('dt', null, name));
    const dd = el('dd', null, value);
    if (name === t('fact.location', '位置') || name === t('fact.project', '工程')) {
      dd.dataset.path = value;
      dd.title = t('detail.copyHint', '点击复制路径');
      dd.addEventListener('click', () => copyText(value));
    }
    facts.append(dd);
  }

  // Older cached shells may lack some controls; the facts panel must still open.
  const beautify = $('detail-beautify');
  if (beautify) beautify.checked = state.settings.beautify;
  const verify = $('detail-verify');
  if (verify) verify.checked = state.settings.verify;
  const advanced = $('detail-advanced');
  if (advanced) advanced.hidden = true;
  const structure = $('detail-structure');
  if (structure) {
    structure.hidden = true;
    structure.textContent = '';
  }
  const out = $('detail-out');
  if (out) {
    out.value = state.settings.defaultOutput || '';
    out.placeholder = state.settings.defaultOutput || t('field.outputPh', '留空使用默认目录');
  }
  const unpack = $('detail-unpack');
  if (unpack) {
    unpack.disabled = false;
    unpack.textContent = t('btn.unpackShort', '还原');
  }
  openModal('modal-detail');
}

async function unpackDetail() {
  if (!detailProgram) return;
  const button = $('detail-unpack');
  button.disabled = true;
  button.textContent = t('btn.unpacking', '还原中…');
  try {
    const data = await call('unpack', {
      args: {
        appid: detailProgram.appid,
        paths: detailProgram.packagePaths || [],
        out: $('detail-out').value.trim() || null,
        beautify: $('detail-beautify').checked,
        verify: $('detail-verify').checked,
        options: {
          keepRaw: $('flag-keep-raw').checked,
          decompileWxml: $('flag-wxml').checked,
          rewriteRequires: $('flag-requires').checked,
          prune: $('flag-prune').checked,
          synthesize: $('flag-synthesize').checked,
          pluginClean: $('flag-plugins').checked,
        },
      },
    });
    detailProgram.extracted = true;
    detailProgram.projectDir = data.projectDir;
    renderPrograms();
    closeModal('modal-detail');
    notify('success', data.projectDir, {
      title: t('toast.unpacked', '还原源码完成'),
      action: { label: t('dl.openFolder', '打开目录'), run: () => revealPath(data.projectDir) },
    });
  } catch (error) {
    notify('error', describe(error), { title: t('toast.unpackFailed', '还原源码失败') });
  } finally {
    button.disabled = false;
    button.textContent = '还原';
  }
}

async function rawDetail() {
  if (!detailProgram) return;
  const button = $('detail-raw');
  button.disabled = true;
  button.textContent = t('btn.exporting', '导出中…');
  try {
    const data = await call('raw_export', {
      paths: detailProgram.packagePaths,
      out: $('detail-out').value.trim() || null,
    });
    notify('success', `${data.files} 个文件 → ${data.outDir}`, {
      title: t('toast.rawDone', '原样导出完成'),
      action: { label: t('dl.openFolder', '打开目录'), run: () => revealPath(data.outDir) },
    });
  } catch (error) {
    notify('error', describe(error), { title: t('toast.rawFailed', '原样导出失败') });
  } finally {
    button.disabled = false;
    button.textContent = t('btn.raw', '原样导出');
  }
}

async function inspectDetail() {
  if (!detailProgram) return;
  const button = $('detail-inspect');
  const box = $('detail-structure');
  button.disabled = true;
  button.textContent = t('btn.loading', '读取中…');
  box.hidden = false;
  box.textContent = '';
  box.append(el('p', 'hint', t('inspect.loading', '正在读取容器结构…')));
  try {
    const containers = await call('inspect', {
      paths: detailProgram.packagePaths,
      wxid: detailProgram.wxid || null,
    });
    box.textContent = '';
    for (const container of containers) {
      const pack = el('div', 'pack');
      pack.append(el('p', 'name', basename(container.path)));
      const facts = el('dl', 'facts');
      for (const [name, value] of [
        [t('fact.version', '格式版本'), container.version],
        [t('fact.files', '文件数'), container.files],
        [t('fact.size', '大小'), bytes(container.size)],
      ]) {
        facts.append(el('dt', null, name));
        facts.append(el('dd', null, String(value)));
      }
      pack.append(facts);
      box.append(pack);
    }
  } catch (error) {
    box.textContent = '';
    box.append(el('p', 'hint', describe(error)));
  } finally {
    button.disabled = false;
    button.textContent = t('btn.inspect', '查看结构');
  }
}

/* ----------------------------------------------------------------- batch */

function selectedPrograms() {
  return state.programs.filter((p) => state.selected.has(key(p.appid, p.version)));
}

function renderBatchList() {
  const list = $('batch-list');
  const chosen = selectedPrograms();
  list.textContent = '';
  $('batch-summary').textContent = chosen.length
    ? tf('batch.summary', '已选 {n} 个小程序，按 appid 分目录导出。', { n: chosen.length })
    : t('batch.none', '尚未选择小程序。');
  for (const program of chosen) {
    const item = el('li');
    item.dataset.appid = program.appid;
    item.append(el('span', null, program.appid));
    item.append(el('span', 'status', t('batch.waiting', '等待')));
    list.append(item);
  }
  $('batch-run').disabled = chosen.length === 0;
  $('batch-run').textContent = t('batch.run', '开始导出');
}

function openBatch() {
  $('batch-out').value = state.settings.defaultOutput || '';
  $('batch-out').placeholder = state.settings.defaultOutput || t('field.outputPh', '留空使用默认目录');
  $('batch-beautify').checked = state.settings.beautify;
  $('batch-verify').checked = state.settings.verify;
  $('batch-status').textContent = '';
  renderBatchList();
  openModal('modal-batch');
}

async function runBatch() {
  const chosen = selectedPrograms();
  if (chosen.length === 0) return;
  const button = $('batch-run');
  button.disabled = true;
  button.textContent = t('btn.exporting', '导出中…');
  $('batch-status').textContent = `0 / ${chosen.length}`;

  const watcher = (event) => {
    if (event.id !== 'batch' || event.phase !== 'progress' || !event.message) return;
    const match = /正在导出 (\S+)/.exec(event.message);
    if (match) {
      for (const item of $('batch-list').children) {
        item.classList.toggle('running', item.dataset.appid === match[1]);
      }
    }
    if (event.done !== undefined && event.done !== null) {
      $('batch-status').textContent = `${event.done} / ${event.total}`;
    }
  };
  const unsubscribe = TAURI.event?.listen
    ? TAURI.event.listen('wxapkg-job', (event) => watcher(event.payload || {}))
    : null;

  try {
    const result = await call('export_batch', {
      args: {
        appids: chosen.map((p) => p.appid),
        out: $('batch-out').value.trim() || null,
        beautify: $('batch-beautify').checked,
        verify: $('batch-verify').checked,
        options: {},
      },
    });
    for (const item of $('batch-list').children) {
      item.classList.remove('running');
      const outcome = result.results.find((r) => r.appid === item.dataset.appid);
      const label = item.querySelector('.status');
      if (outcome && outcome.ok) {
        item.classList.add('ok');
        label.textContent = `完成 · ${outcome.files} 个文件`;
      } else {
        item.classList.add('bad');
        label.textContent = (outcome && outcome.error) || t('state.failed', '失败');
      }
    }
    $('batch-status').textContent = tf('batch.result', '成功 {ok}，失败 {failed}', { ok: result.succeeded, failed: result.failed });
    state.programs.forEach((p) => {
      if (result.results.some((r) => r.ok && r.appid === p.appid)) p.extracted = true;
    });
    renderPrograms();
    if (result.failed > 0) {
      notify('warn', tf('batch.result', '成功 {ok}，失败 {failed}', { ok: result.succeeded, failed: result.failed }), { title: t('batch.partial', '批量导出部分失败') });
    } else {
      notify('success', tf('batch.ok', '成功 {n} 个', { n: result.succeeded }), { title: t('batch.done', '批量导出完成') });
    }
  } catch (error) {
    $('batch-status').textContent = describe(error);
    notify('error', describe(error), { title: t('batch.failed', '批量导出失败') });
  } finally {
    if (typeof unsubscribe === 'function') unsubscribe();
    button.disabled = false;
    button.textContent = '开始导出';
  }
}

/* ---------------------------------------------------------------- remote */

function accountPill(account) {
  const stale = !account.online && account.status && account.status !== 'online';
  return account.online ? t('acct.online', '在线') : stale ? t('acct.stale', '需重新登录') : t('acct.offline', '离线');
}

function renderAccount() {
  const box = $('accounts');
  box.textContent = '';
  const account = state.account;
  if (!account) {
    const empty = el('div', 'empty');
    empty.append(el('strong', null, t('acct.notSignedIn', '尚未登录')));
    empty.append(el('span', null, t('empty.noRecentHint', '扫码后即可读取最近使用的小程序并下载。')));
    box.append(empty);
    const login = el('button', 'btn small', t('acct.relogin', '重新扫码登录'));
    login.type = 'button';
    login.addEventListener('click', openLogin);
    box.append(login);
    return;
  }
  const card = el('div', 'account is-active');
  const avatar = el('span', 'avatar');
  if (account.avatar) {
    const img = el('img');
    img.src = account.avatar;
    img.alt = '';
    img.referrerPolicy = 'no-referrer';
    avatar.append(img);
  } else {
    avatar.textContent = (account.nickName || '·').slice(0, 1);
  }
  const text = el('div');
  text.append(el('div', 'name', account.nickName || t('acct.wechatUser', '微信用户')));
  const pill = el('span', `pill ${account.online ? 'online' : ''}`);
  pill.textContent = accountPill(account);
  const meta = el('div', 'meta');
  meta.append(pill);
  text.append(meta);
  card.append(avatar, text);

  const login = el('button', 'btn small', t('acct.relogin', '重新扫码登录'));
  login.type = 'button';
  login.addEventListener('click', openLogin);
  card.append(login);

  const out = el('button', 'btn small quiet', t('acct.delete', '删除'));
  out.type = 'button';
  out.addEventListener('click', deleteAccount);
  card.append(out);
  box.append(card);
}

async function loadAccounts() {
  const box = $('accounts');
  box.textContent = '';
  box.append(el('span', 'hint', t('acct.loading', '正在读取登录状态…')));
  try {
    const status = await call('ilink_status');
    if (!status.available) {
      box.textContent = '';
      const note = el('div', 'empty');
      note.append(el('strong', null, t('remote.unavailable', '远程下载不可用')));
      note.append(el('span', null, status.reason || t('remote.notReady', '远程引擎未就绪')));
      box.append(note);
      $('btn-recent').disabled = true;
      $('status-remote').textContent = t('status.remoteDown', '远程 不可用');
      $('status-remote').classList.add('bad');
      return;
    }
    state.account = status.account || null;
    $('status-remote').textContent = tf('status.remote', '远程 {impl}', { impl: status.implementation || 'ok' });
    $('status-remote').classList.remove('bad');
    renderAccount();
    for (const warning of status.warnings || []) {
      addLog('warn', warning);
      notify('warn', warning, { title: t('acct.status', '账号状态') });
    }
    $('btn-recent').disabled = !state.account;
  } catch (error) {
    box.textContent = '';
    box.append(el('span', 'hint', describe(error)));
  }
}

async function deleteAccount() {
  const who = state.account ? (state.account.nickName || t('acct.current', '当前账号')) : t('acct.current', '当前账号');
  if (!window.confirm(tf('acct.confirmDelete', '删除账号 {who}？将同时清除图标与最近使用缓存。', { who }))) return;
  try {
    await call('ilink_account_delete');
    notify('success', t('toast.deleted', '账号已删除'));
    state.account = null;
    state.recent = [];
    state.recentLoaded = false;
    state.recentSelected.clear();
    renderRecent();
    await loadAccounts();
  } catch (error) {
    notify('error', describe(error), { title: t('toast.deleteFailed', '删除账号失败') });
  }
}

/* ----------------------------------------------------------------- login */

function stopLoginPolling() {
  if (state.login && state.login.timer) clearInterval(state.login.timer);
  if (state.login && state.login.poll) clearInterval(state.login.poll);
  state.login = null;
}

/** Opens a login session; a confirmed scan replaces whatever account was stored. */
async function beginLogin() {
  stopLoginPolling();
  $('qr').hidden = true;
  $('qr').removeAttribute('src');
  $('qr-loading').hidden = false;
  $('login-hint').textContent = t('qr.hint', '用微信扫描二维码');
  $('qr-left').textContent = '--:--';
  $('login-restart').disabled = true;

  try {
    const view = await call('ilink_login_begin');
    const source = view.qrImage || view.qrContent;
    if (!source) throw t('login.noQr', '上游没有返回二维码');
    $('qr').onerror = () => {
      // The QR is a remote image; a failed fetch must not leave a blank box and no hint.
      $('qr').hidden = true;
      $('qr-loading').hidden = false;
      $('qr-loading').textContent = t('login.qrImageFailed', '二维码图片加载失败，点「重新获取」');
      $('login-hint').textContent = t('login.qrImageFailedShort', '无法加载二维码图片');
    };
    $('qr').src = source;
    $('qr').hidden = false;
    $('qr-loading').hidden = true;
    $('login-hint').textContent = view.qrIsUrl
      ? t('qr.confirm', '用微信扫描二维码并在手机上确认')
      : t('qr.hint', '用微信扫描二维码');

    const expires = Number(view.expiresIn);
    const lifetime = Number.isFinite(expires) && expires > 0 ? expires : 240;
    state.login = {
      sessionId: view.sessionId || 'default',
      loginId: null,
      deadline: Date.now() + lifetime * 1000,
      timer: 0,
      poll: 0,
      done: false,
      lastStatus: '',
      failures: 0,
    };

    const tick = () => {
      if (!state.login) return;
      const left = (state.login.deadline - Date.now()) / 1000;
      $('qr-left').textContent = seconds(left);
      if (left <= 0) {
        $('login-hint').textContent = t('qr.expired', '二维码已过期，请重新获取');
        clearInterval(state.login.timer);
        clearInterval(state.login.poll);
        $('login-restart').disabled = false;
      }
    };
    tick();
    state.login.timer = setInterval(tick, 500);

    state.login.poll = setInterval(async () => {
      if (!state.login || state.login.done) return;
      const session = state.login;
      try {
        const status = await call('ilink_login_poll', { sessionId: session.sessionId });
        session.failures = 0;
        if (status.status !== session.lastStatus) {
          session.lastStatus = status.status;
          addLog('info', tf('login.status', '登录状态：{status}', { status: status.status }));
        }
        if (status.status === 'confirmed') {
          session.done = true;
          clearInterval(session.timer);
          clearInterval(session.poll);
          $('login-hint').textContent = status.nickName ? tf('login.okNamed', '登录成功：{name}', { name: status.nickName }) : t('login.ok', '登录成功');
          notify('success', status.nickName || t('acct.signedIn', '账号已登录'), { title: t('login.ok', '登录成功') });
          setTimeout(() => closeModal('modal-login'), 900);
          state.recentLoaded = false;
          await loadAccounts();
          if (state.view === 'remote') await ensureRecent();
          return;
        }
        if (status.status === 'expired' || status.status === 'cancelled') {
          clearInterval(session.timer);
          clearInterval(session.poll);
          $('login-hint').textContent = status.message || t('qr.invalid', '二维码已失效，请重新获取');
          $('login-restart').disabled = false;
          return;
        }
        if (status.status === 'scanned') {
          $('login-hint').textContent = t('qr.scanned', '已扫描，请在手机上确认');
        }
      } catch (error) {
        session.failures += 1;
        // A single dropped poll is normal; only stop when upstream stays unreachable.
        if (session.failures >= 5) {
          clearInterval(session.poll);
          $('login-hint').textContent = tf('login.pollFailed', '登录状态查询失败：{err}', { err: describe(error) });
          $('login-restart').disabled = false;
        }
      }
    }, 1500);
  } catch (error) {
    $('qr-loading').hidden = true;
    $('login-hint').textContent = describe(error);
    notify('error', describe(error), { title: t('login.qrFailed', '获取二维码失败') });
  } finally {
    $('login-restart').disabled = false;
  }
}

/** Shows the login sheet. */
function openLogin() {
  $('qr').removeAttribute('src');
  $('qr').hidden = true;
  $('qr-loading').hidden = false;
  openModal('modal-login');
  beginLogin();
}

/* ---------------------------------------------------------------- recent */

function renderRecent() {
  const body = $('recent-body');
  const empty = $('remote-empty');
  body.textContent = '';
  const needle = $('filter').value.trim().toLowerCase();
  const rows = state.recent.filter((item) => {
    if (!needle) return true;
    return `${item.nickName || ''} ${item.username || ''} ${item.appid || ''}`
      .toLowerCase()
      .includes(needle);
  });
  empty.hidden = rows.length > 0;
  if (rows.length === 0) {
    if (state.recent.length === 0) {
      renderEmpty(empty, t('empty.noRecent', '还没有最近使用记录'), t('empty.noRecentHint', '先用微信扫码登录，再读取最近使用。'));
    } else {
      renderEmpty(empty, t('empty.noMatch', '没有匹配的结果'), t('empty.noMatchHint', '换个关键字，或清空搜索框。'));
    }
  }

  state.recentRows.clear();
  const fragment = document.createDocumentFragment();
  rows.forEach((item) => {
    const row = el('tr', 'row');
    const id = item.username || item.appid;
    if (state.recentSelected.has(id)) row.classList.add('is-selected');

    const checkCell = el('td', 'c-check');
    const check = el('input');
    check.type = 'checkbox';
    check.checked = state.recentSelected.has(id);
    check.setAttribute('aria-label', `选择 ${item.nickName || id}`);
    check.addEventListener('change', () => {
      if (check.checked) state.recentSelected.add(id);
      else state.recentSelected.delete(id);
      row.classList.toggle('is-selected', check.checked);
      syncRecentSelection();
    });
    checkCell.append(check);

    const iconCell = el('td', 'c-icon');
    const paintIcon = (src) => {
      iconCell.textContent = '';
      iconCell.append(iconNode(src || null));
    };
    if (item.icon) paintIcon(item.icon);
    else loadRecentIcon(item.username).then(paintIcon);

    const nameCell = el('td', null, item.nickName || item.username || '小程序');
    const userCell = el('td', 'mono', item.username || '—');
    const appidCell = el('td', 'mono', item.appid || '—');
    row.append(
      checkCell,
      iconCell,
      nameCell,
      userCell,
      appidCell,
      el('td', 'c-num mono', clock(item.lastUsedAt)),
    );
    state.recentRows.set(item.username || id, { item, nameCell, appidCell, iconCell });

    const actions = el('td', 'c-act');
    const download = el('button', 'btn small', t('btn.downloadRow', '下载'));
    download.type = 'button';
    download.addEventListener('click', (event) => {
      event.stopPropagation();
      openDownloadDialog([{ item }], download);
    });
    actions.append(download);
    row.append(actions);
    row.addEventListener('click', () => showInfo(item));
    row.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      event.stopPropagation();
      recentMenu(event, item);
    });
    fragment.append(row);
  });
  body.append(fragment);
  syncRecentSelection();
}

/** Patches one row when its metadata lands; never re-renders the whole table. */
function applyRecentMeta(meta) {
  const key = meta.username;
  if (!key) return;
  const item = state.recent.find((row) => row.username === key);
  if (item) {
    if (meta.appid) item.appid = meta.appid;
    if (meta.nickName) item.nickName = meta.nickName;
    if (meta.icon) item.icon = meta.icon;
  }
  const view = state.recentRows.get(key);
  if (!view) return;
  if (meta.nickName) view.nameCell.textContent = meta.nickName;
  if (meta.appid) view.appidCell.textContent = meta.appid;
  if (meta.icon) {
    view.iconCell.textContent = '';
    view.iconCell.append(iconNode(meta.icon));
  }
}

/** Asks the backend to resolve names and icons; each row fills in as one arrives. */
async function enrichRecent() {
  const usernames = state.recent.map((item) => item.username).filter(Boolean);
  if (usernames.length === 0) return;
  try {
    await call('ilink_enrich_recent', { usernames });
  } catch {
    // The cache keeps what was resolved; the next refresh picks it up.
  }
}

/** Right-click menu for one recent row; offers the download actions when a record exists. */
function recentMenu(event, item) {
  const record = loadDownloads().find((row) =>
    (item.appid && row.appid === item.appid) || (item.username && row.username === item.username));
  const items = [
    { label: t('menu.detail', '查看详情'), action: () => showInfo(item) },
  ];
  if (record) {
    items.push(
      { label: t('menu.unpack', '还原源码'), action: () => restoreDownload(record).catch((error) => notify('error', describe(error), { title: t('toast.unpackFailed', '还原源码失败') })) },
      { label: t('menu.raw', '原样导出'), action: () => rawExportDownload(record) },
      '-',
      { label: t('menu.openLocation', '打开包路径'), action: () => revealPath(record.outDir) },
    );
  } else {
    items.push({ label: t('btn.downloadRow', '下载'), action: () => openDownloadDialog([{ item }], null) });
  }
  items.push({ label: t('menu.copyAppid', '复制 AppID'), action: () => copyText(item.appid || '') });
  if (item.username) {
    items.push({ label: t('recent.copyUsername', '复制原始 ID'), action: () => copyText(item.username) });
  }
  showMenu(event.clientX, event.clientY, items);
}

function syncRecentSelection() {
  const total = state.recent.length;
  const chosen = state.recent.filter((i) => state.recentSelected.has(i.username || i.appid));
  $('btn-download-selected').disabled = chosen.length === 0;
  $('select-all-recent').checked = total > 0 && chosen.length === total;
  $('select-all-recent').indeterminate = chosen.length > 0 && chosen.length < total;
  $('recent-note').textContent = chosen.length
    ? tf('sel.summary', '已选 {n} / {total}', { n: chosen.length, total })
    : tf('recent.count', '{n} 条记录', { n: total });
}

async function loadRecent() {
  if (!state.account) return;
  if (state.recentLoading) return;
  state.recentLoading = true;
  const button = $('btn-recent');
  button.disabled = true;
  $('recent-note').textContent = t('btn.loading', '读取中…');
  skeletonRows($('recent-body'), 7, 6);
  $('remote-empty').hidden = true;
  try {
    const items = await call('ilink_recent', { page: 1, size: 30 });
    state.recent = items || [];
    state.recentSelected.clear();
    state.recentRows.clear();
    state.recentLoaded = true;
    renderRecent();
    enrichRecent();
  } catch (error) {
    $('recent-body').textContent = '';
    renderEmpty($('remote-empty'), t('recent.failed', '读取失败'), describe(error));
    $('remote-empty').hidden = false;
    $('recent-note').textContent = '—';
    const message = describe(error);
    if (isAuthError(message)) promptRelogin(message);
    else notify('error', message, { title: t('toast.recentFailed', '读取最近使用失败') });
  } finally {
    state.recentLoading = false;
    button.disabled = false;
  }
}

/** Opens the info sheet immediately, then fills it: a stalled network must not look frozen. */
async function showInfo(item) {
  $('modal-detail').classList.add('is-info');
  const localFoot = $('detail-local-foot');
  if (localFoot) localFoot.hidden = true;
  const oldInfoFoot = document.querySelector('#detail-info-foot');
  if (oldInfoFoot) oldInfoFoot.remove();
  $('detail-icon').style.visibility = 'hidden';
  $('detail-id').textContent = item.nickName || item.username || t('detail.title', '小程序信息');
  $('detail-sub').textContent = item.username || item.appid || '';
  const facts = $('detail-facts');
  facts.textContent = '';
  facts.append(el('dt', null, t('detail.status', '状态')));
  const loading = el('dd', 'hint', t('detail.querying', '正在查询…'));
  facts.append(loading);
  $('detail-advanced').hidden = true;
  $('detail-structure').hidden = true;
  const icon = $('detail-icon');
  if (item.icon) {
    icon.src = item.icon;
    icon.style.visibility = 'visible';
  } else if (item.username) {
    loadRecentIcon(item.username).then((src) => {
      if (!src) return;
      icon.src = src;
      icon.style.visibility = 'visible';
    });
  }
  openModal('modal-detail');

  try {
    const info = await call('ilink_info', {
      appid: item.appid || null,
      username: item.username || null,
    });
    $('detail-id').textContent = info.nickName || item.nickName || t('detail.title', '小程序信息');
    $('detail-sub').textContent = info.signature || '';
    facts.textContent = '';
    const stamp = info.clientVersion
      ? `${info.version || 0}(${info.clientVersion})`
      : String(info.version || 0);
    for (const [name, value] of [
      ['APPID', info.appid || '—'],
      [t('fact.origId', '原始 ID'), info.username || '—'],
      [t('fact.version', '线上版本号'), stamp],
      [t('fact.updated', '更新时间'), info.updateTime ? clock(info.updateTime) : '—'],
    ]) {
      facts.append(el('dt', null, name));
      const dd = el('dd', null, value);
      if (name === t('fact.version', '线上版本号')) {
        dd.innerHTML = '';
        dd.append(`${info.version || 0}(`);
        const small = el('span', 'hint', `${info.clientVersion || 0})`);
        dd.append(small);
      }
      facts.append(dd);
    }
    addInfoJsonViewer(info);
    addInfoDownloadAction(item, info);
  } catch (error) {
    const message = describe(error);
    facts.textContent = '';
    facts.append(el('dt', null, t('detail.queryFailed', '查询失败')));
    const dd = el('dd', 'hint', message);
    // A failed lookup must be retryable in place: the refresh button is the same one that
    // opened the sheet, so the user does not have to close and reopen it.
    const retry = el('button', 'btn small', t('btn.retry', '重试'));
    retry.type = 'button';
    retry.addEventListener('click', () => showInfo(item));
    dd.append(' ');
    dd.append(retry);
    facts.append(dd);
    if (isAuthError(message)) promptRelogin(message);
    else notify('error', message, { title: t('toast.infoFailed', '查询信息失败') });
  }
}

/** Adds a read-only, formatted JSON view of the raw attribute payload. */
function addInfoJsonViewer(info) {
  const sheet = document.querySelector('#modal-detail .sheet');
  if (!sheet) return;
  const old = sheet.querySelector('#info-json');
  if (old) old.remove();
  const box = el('pre', 'json-view', JSON.stringify(info.attrs || {}, null, 2));
  box.id = 'info-json';
  box.tabIndex = 0;
  const body = sheet.querySelector('.sheet-body');
  if (body) body.append(box);
}

/** Places the primary download action inside an info sheet. */
function addInfoDownloadAction(item, info) {
  const sheet = document.querySelector('#modal-detail .sheet');
  if (!sheet || sheet.querySelector('#info-download')) return;
  let foot = sheet.querySelector('#detail-info-foot');
  if (!foot) {
    foot = el('footer', 'sheet-foot');
    foot.id = 'detail-info-foot';
    sheet.append(foot);
  }
  foot.textContent = '';
  foot.append(el('span', 'grow'));
  const download = el('button', 'btn primary', t('btn.download', '下载'));
  download.type = 'button';
  download.id = 'info-download';
  download.addEventListener('click', () => {
    closeModal('modal-detail');
    openDownloadDialog([{ item, info }], null);
  });
  foot.append(download);
}

/** Downloads one recent item. */
/** Choices made in the download dialog, remembered across launches. */
function loadDownloadOptions() {
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem('wxapkg-dump.dl-options') || '{}'); } catch { /* defaults */ }
  return {
    out: saved.out !== undefined ? saved.out : (state.settings.defaultOutput || ''),
    unpack: saved.unpack !== undefined ? saved.unpack : true,
    beautify: saved.beautify !== undefined ? saved.beautify : (state.settings.beautify !== false),
    all: saved.all !== undefined ? saved.all : false,
  };
}

/** What the dialog is currently about: one program or a multi-selection. */
let pendingDownloads = null;

/** Opens the per-download options dialog; every entry point (row, menu, bulk) uses it. */
function openDownloadDialog(items, button) {
  if (!items || items.length === 0) return;
  pendingDownloads = { items, button };
  const saved = loadDownloadOptions();
  $('download-target').textContent = items.length === 1
    ? (items[0].item.nickName || items[0].item.username || items[0].item.appid || '')
    : tf('dlg.targets', '共 {n} 个小程序', { n: items.length });
  $('download-out').value = saved.out;
  $('download-unpack').checked = saved.unpack;
  $('download-beautify').checked = saved.beautify;
  $('download-all').checked = saved.all;
  openModal('modal-download');
}

/** Runs the queued download(s) with the dialog's choices. */
async function runDownloadDialog() {
  const options = {
    out: $('download-out').value.trim(),
    unpack: $('download-unpack').checked,
    beautify: $('download-beautify').checked,
    all: $('download-all').checked,
  };
  localStorage.setItem('wxapkg-dump.dl-options', JSON.stringify(options));
  closeModal('modal-download');
  const job = pendingDownloads;
  pendingDownloads = null;
  if (!job) return;
  const run = $('download-run');
  run.disabled = true;
  const records = [];
  let failed = 0;
  for (const { item, info } of job.items) {
    try {
      const record = await downloadItem(item, job.items.length === 1 ? job.button : null, options, info);
      records.push(record);
    } catch (error) {
      const message = describe(error);
      if (/cancel/i.test(message)) {
        notify('warn', message, { title: t('toast.downloadCancelled', '下载已取消') });
      } else if (isAuthError(message)) {
        promptRelogin(message);
        break;
      } else {
        failed += 1;
        notify('error', message, { title: t('toast.downloadFailed', '下载失败') });
      }
    }
  }
  run.disabled = false;
  if (job.items.length > 1) {
    notify(failed ? 'warn' : 'success', tf('batch.result', '成功 {ok}，失败 {failed}', { ok: records.length, failed }), {
      title: failed ? t('toast.batchPartial', '批量下载部分失败') : t('toast.batchDone', '批量下载完成'),
    });
  }
  if (options.unpack) {
    for (const record of records) {
      await restoreDownload(record, options.beautify).catch((error) =>
        notify('error', describe(error), { title: t('toast.unpackFailed', '还原源码失败') }));
    }
  }
}

async function downloadItem(item, button, options, info) {
  const id = (info && (info.appid || info.username)) || item.appid || item.username;
  if (button) {
    button.disabled = true;
    button.textContent = t('btn.downloadingShort', '下载中');
  }
  noteDownloadStart(id, item.nickName || item.username, item.icon || item.iconUrl);
  setTimeout(() => {
    const center = document.getElementById('download-center');
    if (center && center.hidden) {
      center.hidden = false;
      renderDownloads();
    }
  }, 0);
  try {
    // Download and restore are separate calls: the record becomes actionable the moment
    // the last package lands, not after the decompile finishes.
    const data = await call('ilink_download', {
      appid: (info && info.appid) || item.appid || null,
      username: (info && info.username) || item.username || null,
      out: options.out || null,
      unpack: false,
      beautify: false,
      allPackages: options.all,
      id,
    });
    const record = downloadRecord(item, data);
    record.liveId = id;
    record.skipped = (data.report?.packages || []).filter((pack) => pack.skipped).length;
    rememberDownload(record);
    if (button) button.textContent = t('btn.downloaded', '已下载');
    const detail = record.skipped
      ? `${record.outDir} · ${tf('toast.skippedResource', '已跳过 {n} 个资源包；需要时在下载前勾选「资源包」', { n: record.skipped })}`
      : record.outDir;
    notify(record.failed ? 'warn' : 'success', detail, {
      title: record.failed
        ? tf('toast.partialCount', '{ok} 个包成功，{failed} 个失败', { ok: record.total - record.failed, failed: record.failed })
        : tf('toast.downloadCount', '下载完成 · {n} 个包', { n: record.total }),
      action: { label: t('dl.openFolder', '打开目录'), run: () => revealPath(record.outDir) },
    });
    return record;
  } catch (error) {
    finishLiveDownload(id);
    if (button) {
      button.disabled = false;
      button.textContent = t('btn.downloadRow', '下载');
    }
    throw error;
  }
}

/** Whether a failure means "sign in again" rather than "this call went wrong". */
function isAuthError(message) {
  return /未登录|登录态|登录会话|not_logged_in|session_expired|no App Store account|请先扫码/i
    .test(String(message));
}

/** Session is unusable: refresh the card (the store marked it stale) and offer a scan. */
function promptRelogin(message) {
  loadAccounts();
  notify('warn', message, {
    title: t('toast.needLogin', '请重新扫码登录'),
    action: { label: t('acct.relogin', '重新扫码登录'), run: () => openLogin() },
  });
}

/** Downloads every selected recent item through the same options dialog. */
function downloadSelected() {
  const chosen = state.recent.filter((i) => state.recentSelected.has(i.username || i.appid));
  if (chosen.length === 0) return;
  openDownloadDialog(chosen.map((item) => ({ item })), null);
}

/* --------------------------------------------------------- remote query */

function queryValue() {
  return $('remote-query').value.trim();
}

async function queryInfo() {
  const value = queryValue();
  if (!(/^wx[0-9a-f]{16}$/i.test(value) || /^gh_[0-9a-z]+(@app)?$/i.test(value))) {
    notify('warn', t('query.invalid', '请输入 APPID 或者原始 ID'));
    $('remote-query').focus();
    return;
  }
  const appid = /^wx[0-9a-f]{16}$/i.test(value) ? value.toLowerCase() : null;
  const username = appid ? null : value;
  await showInfo({ appid, username, nickName: value });
}

function downloadQuery() {
  const value = queryValue();
  if (!(/^wx[0-9a-f]{16}$/i.test(value) || /^gh_[0-9a-z]+(@app)?$/i.test(value))) {
    notify('warn', t('query.invalid', '请输入 APPID 或者原始 ID'));
    $('remote-query').focus();
    return;
  }
  const appid = /^wx[0-9a-f]{16}$/i.test(value) ? value.toLowerCase() : null;
  openDownloadDialog([{ item: { appid, username: appid ? null : value, nickName: value } }], $('btn-download-one'));
}

/* ----------------------------------------------------------- environment */

function envRows(env) {
  const roots = (env.roots || []).map((root) => `${root.path}${root.readable ? '' : '（不可读）'}`)
    .join('\n');
  return [
    [t('env.version', '版本'), `${env.version} · ${env.target}`],
    [t('env.system', '系统'), osLabel(env.os, env.arch)],
    [t('env.wechatData', '微信数据'), roots || t('env.notFound', '未找到')],
    [t('env.output', '输出目录'), env.workspace],
    [t('env.dataDir', '数据目录'), env.configDir || env.storageDir || '—'],
    [t('env.remote', '远程引擎'), env.remoteFound ? t('env.remoteNative', '内置实现') : t('env.notFound', '未找到')],
    ['Node', env.nodeFound ? t('env.available', '可用') : t('env.unavailable', '不可用')],
  ];
}

function renderEnv(env) {
  const facts = $('env-facts');
  facts.textContent = '';
  for (const [name, value] of envRows(env)) {
    facts.append(el('dt', null, name));
    const dd = el('dd', null, value);
    dd.style.whiteSpace = 'pre-line';
    if (name === t('env.wechatData', '微信数据') || name === t('env.output', '输出目录') || name === t('env.dataDir', '数据目录')) dd.dataset.path = value.split('\n')[0];
    facts.append(dd);
  }

  $('status-os').textContent = osLabel(env.os, env.arch);
  const readable = (env.roots || []).filter((root) => root.readable);
  const tail = (path) => String(path).split(/[\\/]/).filter(Boolean).slice(-3).join('/');
  $('status-roots').textContent = readable.length ? tail(readable[0].path) : t('env.notFoundShort', '未找到微信数据');
  $('status-roots').title = (env.roots || []).map((root) => root.path).join('\n');

  const permission = $('status-permission');
  const button = $('env-permission');
  if (env.permission === 'denied') {
    permission.hidden = false;
    permission.className = 'chip bad';
    permission.textContent = t('status.denied', '文件访问被拒绝');
    button.hidden = false;
  } else {
    permission.hidden = true;
    button.hidden = true;
  }
}

async function loadEnvironment() {
  try {
    const env = await call('environment');
    state.env = env;
    $('version').textContent = env.version;
    const aboutVersion = document.getElementById('about-version');
    if (aboutVersion) aboutVersion.textContent = `wxapkg-dump ${env.version}`;
    renderEnv(env);
  } catch (error) {
    $('status-os').textContent = describe(error);
  }
}

async function loadCache() {
  try {
    const cache = await call('cache_report');
    $('cache-facts').textContent = tf('cache.facts', '{n} 个还原产物 · {size}', { n: cache.projects.length, size: bytes(cache.totalBytes) });
  } catch (error) {
    $('cache-facts').textContent = describe(error);
  }
}

async function loadSettings() {
  try {
    const settings = await call('get_settings');
    state.settings = { columns: {}, ...settings };
    applyTheme(settings.theme || localStorage.getItem(THEME_STORAGE) || 'system');
    applyColumns();
  } catch (error) {
    addLog('error', describe(error));
  }
}

async function saveSettings({ silent } = {}) {
  try {
    state.settings = await call('save_settings', { settings: state.settings });
    applyTheme(state.settings.theme);
    if (!silent) notify('success', t('toast.saved', '设置已保存'));
  } catch (error) {
    notify('error', describe(error), { title: t('toast.saveFailed', '保存设置失败') });
  }
}

async function openSettings() {
  $('set-output').value = state.settings.defaultOutput || '';
  $('set-beautify').checked = state.settings.beautify;
  $('set-verify').checked = state.settings.verify;
  $('set-concurrency').value = state.settings.concurrency || 3;
  applyTheme(state.settings.theme || 'system');
  if (!state.env) await loadEnvironment();
  else renderEnv(state.env);
  loadCache();
  refreshService();
  openModal('modal-settings');
}

/* --------------------------------------------------------------- service */

async function refreshService() {
  try {
    const status = await call('server_status');
    $('svc-state').textContent = status.running
      ? tf('svc.running', '运行中 · http://{url} · 文档 /docs', { url: status.address })
      : t('svc.stopped', '未运行');
    $('svc-start').disabled = status.running;
    $('svc-stop').disabled = !status.running;
  } catch (error) {
    $('svc-state').textContent = describe(error);
  }
}

async function startService() {
  const host = $('svc-host').value.trim() || '127.0.0.1';
  const port = Number($('svc-port').value) || 0;
  const key = $('svc-key').value.trim();
  if (!port) {
    notify('warn', t('svc.badPort', '端口无效'));
    return;
  }
  const loopback = ['127.0.0.1', 'localhost', '::1'].includes(host);
  if (!loopback && !window.confirm(`将 API 暴露在 ${host}:${port}？任何能访问该地址的人都可以读写本机文件。${key ? '' : '建议设置访问密钥。'}`)) {
    return;
  }
  $('svc-start').disabled = true;
  try {
    const status = await call('server_start', { host, port, key: key || null });
    notify('success', `http://${status.address} · /docs`, { title: t('svc.started', 'API 服务已启动') });
  } catch (error) {
    notify('error', describe(error), { title: t('svc.startFailed', '启动 API 服务失败') });
  } finally {
    refreshService();
  }
}

async function stopService() {
  $('svc-stop').disabled = true;
  try {
    await call('server_stop');
    notify('info', t('svc.stoppedToast', 'API 服务已停止'));
  } catch (error) {
    notify('error', describe(error));
  } finally {
    refreshService();
  }
}

/* ----------------------------------------------------------------- drawer */

function toggleDrawer(show) {
  state.drawer = show === undefined ? !state.drawer : show;
  $('drawer').hidden = !state.drawer;
  $('btn-logs').setAttribute('aria-expanded', String(state.drawer));
  if (state.drawer) {
    $('log-dot').hidden = true;
    $('log-list').scrollTop = $('log-list').scrollHeight;
  }
}

/* ------------------------------------------------------------------ pick */

async function pickFolder(inputId) {
  try {
    const choice = await call('pick_folder', { title: t('pick.output', '选择输出目录') });
    if (choice) {
      $(inputId).value = choice;
      if (inputId === 'set-output') {
        state.settings.defaultOutput = choice;
      }
    }
  } catch (error) {
    notify('error', describe(error), { title: t('pick.failed', '选择目录失败') });
  }
}

/* -------------------------------------------------------------- bootstrap */

function bind() {
  for (const tab of document.querySelectorAll('.tab')) {
    tab.addEventListener('click', () => showView(tab.dataset.view));
  }
  for (const seg of document.querySelectorAll('#theme-group .seg')) {
    seg.addEventListener('click', () => applyTheme(seg.dataset.theme));
  }

  for (const seg of document.querySelectorAll('#lang-group .seg')) {
    seg.addEventListener('click', () => applyLang(seg.dataset.lang));
  }

  $('btn-refresh').addEventListener('click', () => loadPrograms(true));
  $('btn-settings').addEventListener('click', openSettings);
  $('btn-logs').addEventListener('click', () => toggleDrawer());
  $('log-clear').addEventListener('click', () => {
    $('log-list').textContent = '';
    state.logCount = 0;
    $('log-dot').hidden = true;
  });

  $('filter').addEventListener('input', () => {
    if (state.view === 'local') renderPrograms();
    else renderRecent();
  });

  $('select-all').addEventListener('change', () => {
    const ids = selectableIds();
    if ($('select-all').checked) ids.forEach((id) => state.selected.add(id));
    else ids.forEach((id) => state.selected.delete(id));
    renderPrograms();
  });

  $('th-updated').addEventListener('click', (event) => {
    if (event.target.closest('.grip')) return; // the grip resizes, it does not sort
    state.sort = { key: 'updated', desc: !(state.sort?.key === 'updated' ? state.sort.desc : true) };
    syncSortHeader();
    renderPrograms();
  });

  $('btn-downloads').addEventListener('click', (event) => {
    event.stopPropagation();
    const panel = $('download-center');
    panel.hidden = !panel.hidden;
    if (!panel.hidden) {
      renderDownloads();
      renderDownloadBadge();
    }
  });
  document.addEventListener('click', (event) => {
    const panel = $('download-center');
    if (!panel.hidden && !panel.contains(event.target) && !event.target.closest('#btn-downloads')) {
      panel.hidden = true;
    }
    // The log drawer closes on any click outside it, the same way the download center does.
    if (state.drawer && !$('drawer').contains(event.target) && !event.target.closest('#btn-logs')) {
      toggleDrawer(false);
    }
  });
  $('download-clear').addEventListener('click', (event) => {
    event.stopPropagation();
    // Only records are wiped; running downloads stay visible and no file is touched.
    localStorage.setItem(DOWNLOADS_STORAGE, '[]');
    renderDownloads();
    renderDownloadBadge();
  });

  $('btn-unpack-file').addEventListener('click', async () => {
    $('unpack-file').value = '';
    $('unpack-key').value = '';
    $('unpack-report')?.classList.remove('hint');
    openModal('modal-unpack');
  });
  $('unpack-pick').addEventListener('click', async () => {
    const choice = await call('pick_wxapkg', { title: t('pick.wxapkg', '选择 wxapkg 文件') }).catch(() => null);
    if (choice) $('unpack-file').value = choice;
  });
  $('unpack-run').addEventListener('click', async () => {
    const path = $('unpack-file').value.trim();
    if (!path) { notify('warn', t('pick.needWxapkg', '先选择一个 wxapkg 文件')); return; }
    const button = $('unpack-run');
    button.disabled = true;
    button.textContent = t('unpack.running', '解包中…');
    try {
      const data = await call('unpack_files', {
        paths: [path],
        key: $('unpack-key').value.trim() || null,
        out: null,
        folder: null,
        beautify: $('unpack-beautify').checked,
      });
      const projects = data.projects || [];
      const message = projects.length > 1
        ? tf('unpack.multi', '已还原 {n} 个小程序，各自一个 appid 目录', { n: projects.length })
        : data.projectDir;
      notify('success', message, {
        title: t('toast.unpacked', '还原源码完成'),
        action: { label: t('menu.openProject', '打开目录'), run: () => revealPath(data.projectDir) },
      });
      closeModal('modal-unpack');
      await loadPrograms(false);
    } catch (error) {
      const message = describe(error);
      if (/密钥|key|V1MMWX|missing_decryption_key/i.test(message)) {
        $('unpack-key').focus();
        notify('warn', t('unpack.needKey', '该容器已加密，请输入解密密钥（小程序 APPID）'), { title: t('unpack.keyRequired', '需要密钥') });
      } else {
        notify('error', message, { title: t('toast.unpackFailed', '还原源码失败') });
      }
    } finally {
      button.disabled = false;
      button.textContent = t('unpack.run', '开始解包');
    }
  });

  $('btn-decrypt-folder').addEventListener('click', () => {
    $('decrypt-report').hidden = true;
    openModal('modal-decrypt');
  });
  $('decrypt-pick').addEventListener('click', async () => {
    const choice = await call('pick_folder', { title: t('pick.folder', '选择包含 wxapkg 的文件夹') }).catch(() => null);
    if (choice) $('decrypt-folder').value = choice;
  });
  $('decrypt-run').addEventListener('click', async () => {
    const folder = $('decrypt-folder').value.trim();
    if (!folder) { notify('warn', t('pick.needFolder', '先选择一个文件夹')); return; }
    const keys = $('decrypt-keys').value.split(/[\n,;]+/).map((k) => k.trim()).filter(Boolean);
    const button = $('decrypt-run');
    button.disabled = true;
    button.textContent = t('decrypt.running', '解密中…');
    try {
      const result = await call('decrypt_folder', { folder, keys, out: null });
      const report = $('decrypt-report');
      report.hidden = false;
      report.textContent = `解密 ${result.decrypted} · 明文 ${result.plain} · 失败 ${result.failed} → ${result.outDir}`;
      const failed = result.outcomes.filter((o) => !o.ok);
      for (const item of failed.slice(0, 5)) addLog('error', `${item.file.split('/').pop()}：${item.note}`);
      notify(result.failed ? 'warn' : 'success', tf('decrypt.result', '解密 {n}，明文 {p}，失败 {f}', { n: result.decrypted, p: result.plain, f: result.failed }), {
        title: result.failed ? t('decrypt.partial', '部分文件解密失败') : t('decrypt.done', '文件夹解密完成'),
        action: { label: t('menu.openProject', '打开目录'), run: () => revealPath(result.outDir) },
      });
    } catch (error) {
      notify('error', describe(error), { title: t('decrypt.failed', '解密失败') });
    } finally {
      button.disabled = false;
      button.textContent = t('decrypt.run', '开始解密');
    }
  });

  $('btn-invert').addEventListener('click', () => {
    for (const id of selectableIds()) {
      if (state.selected.has(id)) state.selected.delete(id);
      else state.selected.add(id);
    }
    renderPrograms();
  });

  $('btn-export-selected').addEventListener('click', openBatch);
  $('btn-batch').addEventListener('click', openBatch);
  $('batch-run').addEventListener('click', runBatch);
  $('batch-pick').addEventListener('click', () => pickFolder('batch-out'));

  const onDetail = (id, fn) => {
    const node = $(id);
    if (node) node.addEventListener('click', fn);
  };
  onDetail('detail-unpack', unpackDetail);
  onDetail('detail-raw', rawDetail);
  onDetail('detail-inspect', inspectDetail);
  onDetail('detail-pick', () => pickFolder('detail-out'));
  onDetail('detail-reveal', () => {
    if (!detailProgram) return;
    revealPath(detailProgram.projectDir || detailProgram.location);
  });
  onDetail('detail-advanced-toggle', () => {
    const advanced = $('detail-advanced');
    if (advanced) advanced.hidden = !advanced.hidden;
  });

  $('btn-recent').addEventListener('click', () => loadRecent());
  $('btn-download-selected').addEventListener('click', downloadSelected);
  $('btn-info').addEventListener('click', queryInfo);
  $('btn-download-one').addEventListener('click', downloadQuery);
  $('remote-query').addEventListener('keydown', (event) => {
    if (event.key === 'Enter') queryInfo();
  });
  $('select-all-recent').addEventListener('change', () => {
    if ($('select-all-recent').checked) {
      state.recent.forEach((item) => state.recentSelected.add(item.username || item.appid));
    } else {
      state.recentSelected.clear();
    }
    renderRecent();
  });

  $('login-restart').addEventListener('click', () => beginLogin());

  $('settings-save').addEventListener('click', async () => {
    state.settings = {
      ...state.settings,
      defaultOutput: $('set-output').value.trim(),
      beautify: $('set-beautify').checked,
      verify: $('set-verify').checked,
      concurrency: Math.min(10, Math.max(1, Number($('set-concurrency').value) || 3)),
      theme: document.querySelector('#theme-group .seg.is-active')?.dataset.theme || 'system',
      lang: document.querySelector('#lang-group .seg.is-active')?.dataset.lang || 'zh',
    };
    await saveSettings();
  });
  $('set-pick').addEventListener('click', () => pickFolder('set-output'));
  $('svc-start').addEventListener('click', startService);
  $('svc-stop').addEventListener('click', stopService);
  $('env-permission').addEventListener('click', async () => {
    const opened = await call('open_privacy_settings').catch(() => false);
    if (opened) notify('info', t('perm.opened', '已打开「完全磁盘访问权限」设置'));
    else notify('warn', t('perm.missing', '当前系统没有该设置页'));
  });
  $('env-reveal').addEventListener('click', () => {
    if (state.env?.workspace) revealPath(state.env.workspace);
  });
  $('cache-purge').addEventListener('click', async () => {
    if (!window.confirm(t('cache.confirm', '删除工作区里由本程序产生的还原产物？'))) return;
    try {
      const result = await call('cache_purge', { includeTemp: false });
      notify('success', tf('cache.purged', '删除 {n} 个目录，释放 {size}', { n: result.removed, size: bytes(result.freedBytes) }));
      loadCache();
      loadPrograms(true);
    } catch (error) {
      notify('error', describe(error), { title: t('cache.failed', '清理失败') });
    }
  });

  document.addEventListener('contextmenu', (event) => {
    const target = event.target.closest('[data-path]');
    if (!target) return;
    event.preventDefault();
    const path = target.dataset.path;
    showMenu(event.clientX, event.clientY, [
      { label: t('dl.openFolder', '打开所在目录'), action: () => revealPath(path) },
      { label: t('btn.copyPath', '复制路径'), action: () => copyText(path) },
    ]);
  });
}

/** Persists UI-side failures; without this a broken view leaves nothing on disk. */
function wireErrorLog() {
  const send = (text) => {
    if (TAURI && TAURI.core && typeof TAURI.core.invoke === 'function') {
      call('log_js_error', { message: text }).catch(() => {});
    }
  };
  window.addEventListener('error', (event) => {
    send(`${event.message} @ ${event.filename || '?'}:${event.lineno || 0}`);
  });
  window.addEventListener('unhandledrejection', (event) => {
    const reason = event.reason;
    send(`unhandled rejection: ${reason && reason.message ? reason.message : String(reason)}`);
  });
}

/* --------------------------------------------------------------- social */

/** Opens an https link with the system handler (default browser, no console window). */
function openExternal(url) {
  const fallback = () => {
    const win = window.open(url, '_blank', 'noopener');
    if (!win) notify('error', url, { title: t('toast.openFailed', '无法打开系统浏览器') });
  };
  if (TAURI && TAURI.core && typeof TAURI.core.invoke === 'function') {
    call('open_url', { url }).catch(fallback);
  } else {
    fallback();
  }
}

/** The WeChat contact is copied, not linked: there is no reliable wechat:// URL scheme. */
function copyWechatId() {
  copyText('RSCompanyCEO');
}

function wireSocial() {
  for (const id of ['btn-wechat', 'about-wechat', 'welcome-wechat']) {
    const node = document.getElementById(id);
    if (node) node.addEventListener('click', (event) => { event.preventDefault(); copyWechatId(); });
  }
  for (const id of ['about-github', 'about-telegram', 'welcome-github', 'welcome-telegram']) {
    const node = document.getElementById(id);
    if (node) node.addEventListener('click', (event) => { event.preventDefault(); openExternal(node.href); });
  }
}

function openWelcome() {
  openModal('modal-welcome');
}

async function bootstrap() {
  for (const node of document.querySelectorAll('[data-i18n]')) {
    zhDefaults.set(node.dataset.i18n, node.textContent);
  }
  for (const node of document.querySelectorAll('[data-i18n-ph]')) {
    zhDefaults.set(node.dataset.i18nPh + '.ph', node.placeholder);
  }
  for (const node of document.querySelectorAll('[data-i18n-title]')) {
    zhDefaults.set(node.dataset.i18nTitle + '.title', node.title);
  }
  for (const node of document.querySelectorAll('[data-i18n-alt]')) {
    zhDefaults.set(node.dataset.i18nAlt + '.alt', node.alt);
  }
  for (const node of document.querySelectorAll('[data-i18n-aria]')) {
    zhDefaults.set(node.dataset.i18nAria + '.aria', node.getAttribute('aria-label') || '');
  }
  for (const node of document.querySelectorAll('[data-i18n-col]')) {
    if (node.firstChild) zhDefaults.set(node.dataset.i18nCol, node.firstChild.textContent);
  }
  zhDefaults.set('filter.ph', $('filter').placeholder);
  applyTheme(localStorage.getItem(THEME_STORAGE) || 'system');
  applyLang(currentLang());
  syncSortHeader();
  renderDownloadBadge();
  TAURI.event?.listen?.('wxapkg-recent-meta', (event) => {
    applyRecentMeta((event && event.payload) || {});
  });
  TAURI.event?.listen?.('wxapkg-download-progress', (event) => {
    const p = event.payload || {};
    if (p.__start) {
      noteDownloadStart(p.id, p.id);
      return;
    }
    if (!p.total) return; // a phase with no work must not paint 0/0 over real progress
    noteDownloadProgress(p.id || t('dl.untitled', '小程序'), p.done || 0, p.total, p.module || '');
  });
  wireModals();
  wireSocial();
  wireErrorLog();
  setupColumnResize();
  bind();
  const start = $('welcome-start');
  if (start) start.addEventListener('click', () => closeModal('modal-welcome'));
  const runDownload = $('download-run');
  if (runDownload) runDownload.addEventListener('click', () => { runDownloadDialog(); });
  const pickDownload = $('download-pick');
  if (pickDownload) {
    pickDownload.addEventListener('click', async () => {
      try {
        const choice = await call('pick_folder', { title: t('pick.output', '选择输出目录') });
        if (choice) $('download-out').value = choice;
      } catch (error) {
        notify('error', describe(error), { title: t('pick.failed', '选择目录失败') });
      }
    });
  }
  openWelcome();

  if (TAURI.event && typeof TAURI.event.listen === 'function') {
    TAURI.event.listen('wxapkg-log', (event) => {
      const payload = (event && event.payload) || {};
      addLog(payload.kind, payload.line);
    });
    TAURI.event.listen('wxapkg-job', (event) => jobUpdate((event && event.payload) || {}));
  }

  await loadSettings();
  renderRecent();
  showView('local');
  bootPrograms();
  loadEnvironment();
  loadAccounts();
  refreshService();
}

document.addEventListener('DOMContentLoaded', bootstrap);
