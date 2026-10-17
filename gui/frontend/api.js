// Thin typed layer over the Tauri backend.

const T = window.__TAURI__;

export class AppError extends Error {
  constructor(raw) {
    const e = typeof raw === 'string' ? { message: raw } : raw || {};
    super(e.message || 'Something went wrong');
    this.code = e.code || 'error';
  }
}

async function call(cmd, args) {
  try {
    return await T.core.invoke(cmd, args);
  } catch (e) {
    throw new AppError(e);
  }
}

export const api = {
  boot: () => call('cmd_boot'),
  setCredentials: (clientId, clientSecret) => call('cmd_set_credentials', { clientId, clientSecret }),
  importCredentialsFile: (path) => call('cmd_import_credentials_file', { path }),
  accounts: (force = false) => call('cmd_get_accounts', { force }),
  addAccount: (hint = null) => call('cmd_add_account', { hint }),
  reconnect: (email) => call('cmd_reconnect_account', { email }),
  cancelAdd: () => call('cmd_cancel_add_account'),
  removeAccount: (email) => call('cmd_remove_account', { email }),
  sync: (account = null) => call('cmd_force_sync', { account }),
  view: (id) => call('cmd_view', { id }),
  search: (query) => call('cmd_search', { query }),
  media: (offset, limit) => call('cmd_list_media', { offset, limit }),
  mkdir: (name, parentId) => call('cmd_create_folder', { name, parentId }),
  rename: (id, name) => call('cmd_rename', { id, name }),
  trash: (ids) => call('cmd_trash', { ids }),
  move: (ids, newParentId) => call('cmd_move', { ids, newParentId }),
  upload: (paths, parentId) => call('cmd_upload', { paths, parentId }),
  download: (ids, destDir) => call('cmd_download', { ids, destDir }),
  transfers: () => call('cmd_transfers'),
  cancelTransfer: (id) => call('cmd_cancel_transfer', { id }),
  retryTransfer: (id) => call('cmd_retry_transfer', { id }),
  clearTransfers: () => call('cmd_clear_transfers'),
  trashList: () => call('cmd_list_trash'),
  restore: (account, id) => call('cmd_restore_trashed', { account, id }),
  deleteForever: (account, id) => call('cmd_delete_trashed', { account, id }),
  emptyTrash: (account = null) => call('cmd_empty_trash', { account }),
  settings: () => call('cmd_get_settings'),
  updateSettings: (update) => call('cmd_update_settings', { update }),
  clearCache: () => call('cmd_clear_cache'),
  openExternal: (url) => call('cmd_open_external', { url }),
  openPath: (path) => call('cmd_open_path', { path }),
};

export const events = {
  engine: (cb) => T.event.listen('cloudy://event', (e) => cb(e.payload)),
  authUrl: (cb) => T.event.listen('cloudy://auth-url', (e) => cb(e.payload)),
  drag: (cb) => {
    const subs = ['enter', 'over', 'drop', 'leave'].map((k) =>
      T.event.listen(`tauri://drag-${k}`, (e) => cb(k, e.payload || {})));
    return Promise.all(subs);
  },
};

export const dialogs = {
  files: async () => {
    const r = await T.dialog.open({ multiple: true, directory: false, title: 'Choose files to upload' });
    return r ? [].concat(r) : [];
  },
  folder: async (title = 'Choose a folder') => {
    const r = await T.dialog.open({ multiple: false, directory: true, title });
    return r || null;
  },
  json: async () => {
    const r = await T.dialog.open({ multiple: false, directory: false, title: 'Choose the downloaded credentials file', filters: [{ name: 'JSON', extensions: ['json'] }] });
    return r || null;
  },
};

/** Status helpers for Rust's externally-tagged enums ("Idle" | {"Error": "msg"}). */
export const statusKey = (s) => (typeof s === 'string' ? s : Object.keys(s || {})[0] || 'Idle');
export const statusMsg = (s) => (s && typeof s === 'object' ? Object.values(s)[0] : '');
