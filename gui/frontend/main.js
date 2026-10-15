const { invoke } = window.__TAURI__.core;

// App State
const state = {
    currentView: 'pool',
    currentPath: [{ id: 'root', name: 'Unified Pool' }],
    files: [],
    accounts: [],
    transfers: [],
    settings: null,
    viewMode: 'grid',
    sortBy: 'name',
    sortDir: 'asc',
    contextMenuTarget: null,
};

// DOM Elements
const els = {
    setupWizard: document.getElementById('setup-wizard'),
    clientIdInput: document.getElementById('client-id'),
    clientSecretInput: document.getElementById('client-secret'),
    saveCredsBtn: document.getElementById('save-creds-btn'),
    setupError: document.getElementById('setup-error'),
    
    navItems: document.querySelectorAll('.nav-item'),
    views: document.querySelectorAll('.view-container'),
    
    accountsList: document.getElementById('accounts-list'),
    fileList: document.getElementById('file-list'),
    breadcrumbs: document.getElementById('breadcrumbs'),
    
    emptyState: document.getElementById('empty-state'),
    loadingState: document.getElementById('loading-state'),
    
    contextMenu: document.getElementById('context-menu'),
    toastContainer: document.getElementById('toast-container'),
    
    previewModal: document.getElementById('preview-modal'),
    previewTitle: document.getElementById('preview-title'),
    previewBody: document.getElementById('preview-body'),
    
    newFolderModal: document.getElementById('new-folder-modal'),
    newFolderName: document.getElementById('new-folder-name'),
    
    searchInput: document.getElementById('search-input'),
    clearSearchBtn: document.getElementById('clear-search'),
    toggleViewBtn: document.getElementById('toggle-view-btn'),
    
    dropOverlay: document.getElementById('drop-overlay'),

    photosGrid: document.getElementById('photos-grid'),
    photosEmptyState: document.getElementById('photos-empty-state'),
    photosLoadingState: document.getElementById('photos-loading-state'),
    photosCountBadge: document.getElementById('photos-count-badge'),
};

// Initialization
async function init() {
    try {
        const hasCreds = await invoke('cmd_has_client_credentials');
        if (!hasCreds) {
            els.setupWizard.classList.remove('hidden');
        }
        await loadInitialData();
        setupEventListeners();
        setupDragAndDrop();
        applyTheme();
        
        window.addEventListener('hashchange', handleRoute);
        handleRoute();
        
        setInterval(updateTransfers, 2500);
        setInterval(updateQuotas, 10000);
    } catch (err) {
        showToast(`Initialization error: ${err}`, 'error');
    }
}

async function loadInitialData() {
    try {
        await Promise.all([
            fetchAccounts(),
            fetchSettings(),
            loadFiles(getCurrentFolderId())
        ]);
    } catch (err) {
        console.error("Failed to load initial data", err);
    }
}

function handleRoute() {
    const hash = window.location.hash || '#pool';
    const view = hash.replace('#', '').split('/')[0];
    
    if (['pool', 'photos', 'transfers', 'settings'].includes(view)) {
        state.currentView = view;
        updateViewVisibility();
        if (view === 'settings') {
            fetchSettings();
        } else if (view === 'photos') {
            loadMediaFiles();
        }
    }
}

function updateViewVisibility() {
    els.navItems.forEach(item => {
        item.classList.toggle('active', item.dataset.view === state.currentView);
    });
    
    els.views.forEach(view => {
        view.classList.toggle('active', view.id === `view-${state.currentView}`);
        view.classList.toggle('hidden', view.id !== `view-${state.currentView}`);
    });
}

async function fetchAccounts() {
    try {
        const accounts = await invoke('cmd_list_accounts');
        const quotas = await invoke('cmd_get_all_quotas');
        
        state.accounts = accounts.map(email => {
            const quota = quotas.find(q => q.account === email);
            return { email, quota };
        });
        renderAccounts();
        renderSettingsAccounts();
    } catch (err) {
        console.error("Failed to fetch accounts", err);
    }
}

async function updateQuotas() {
    if (state.currentView !== 'settings' && state.currentView !== 'pool') return;
    try {
        const quotas = await invoke('cmd_get_all_quotas');
        state.accounts.forEach(acc => {
            const quota = quotas.find(q => q.account === acc.email);
            if (quota) acc.quota = quota;
        });
        renderAccounts();
    } catch (e) {}
}

async function loadFiles(nodeId, query = null) {
    els.fileList.innerHTML = '';
    els.emptyState.classList.add('hidden');
    els.loadingState.classList.remove('hidden');
    
    try {
        if (query) {
            state.files = await invoke('cmd_search_files', { query });
        } else {
            state.files = await invoke('cmd_list_files', { nodeId: nodeId });
        }
        renderFiles();
    } catch (err) {
        showToast(`Error loading files: ${err}`, 'error');
        state.files = [];
        renderFiles();
    } finally {
        els.loadingState.classList.add('hidden');
    }
}

async function updateTransfers() {
    if (state.currentView !== 'transfers') return;
    try {
        state.transfers = await invoke('cmd_get_transfers');
        renderTransfers();
    } catch (e) {}
}

async function fetchSettings() {
    try {
        state.settings = await invoke('cmd_get_settings');
        document.getElementById('setting-cache').value = state.settings.cache_size_bytes;
        document.getElementById('setting-cache-val').textContent = formatBytes(state.settings.cache_size_bytes);
        document.getElementById('setting-sync').value = state.settings.sync_interval_secs;
        
        const policySelect = document.getElementById('setting-upload-policy');
        policySelect.value = state.settings.upload_policy.split(':')[0];

        const credsInfo = await invoke('cmd_get_client_credentials');
        const badge = document.getElementById('creds-status-badge');
        const settingsClientId = document.getElementById('settings-client-id');
        
        if (credsInfo && credsInfo.has_credentials) {
            badge.textContent = 'Configured ✓';
            badge.style.background = 'var(--accent-success-container)';
            badge.style.color = 'var(--accent-success)';
            if (settingsClientId && credsInfo.client_id) {
                settingsClientId.value = credsInfo.client_id;
            }
        } else {
            badge.textContent = 'Not Configured ⚠️';
            badge.style.background = 'var(--accent-danger-container)';
            badge.style.color = 'var(--accent-danger)';
        }
    } catch (e) {
        console.error("Settings error", e);
    }
}

const AVATAR_COLORS = ['#1A73E8', '#00897B', '#1E8E3E', '#8E24AA', '#E8710A', '#D93025'];
function getAvatarColor(str) {
    let hash = 0;
    for (let i = 0; i < str.length; i++) hash = str.charCodeAt(i) + ((hash << 5) - hash);
    return AVATAR_COLORS[Math.abs(hash) % AVATAR_COLORS.length];
}

function renderAccounts() {
    if (state.accounts.length === 0) {
        els.accountsList.innerHTML = `
            <div style="padding: 10px; text-align: center; color: var(--text-tertiary); font-size: 0.8rem;">
                No accounts connected.<br>Click <strong>+</strong> above to add one.
            </div>
        `;
        return;
    }

    let totalUsage = 0;
    let totalLimit = 0;

    const cardsHtml = state.accounts.map(acc => {
        const initial = (acc.email[0] || 'U').toUpperCase();
        const avatarBg = getAvatarColor(acc.email);
        let quotaHtml = '';
        
        if (acc.quota) {
            totalUsage += acc.quota.usage;
            totalLimit += acc.quota.limit;
            const percent = acc.quota.limit > 0 ? Math.min(100, (acc.quota.usage / acc.quota.limit) * 100) : 0;
            quotaHtml = `
                <div class="quota-bar-bg">
                    <div class="quota-bar-fill" style="width: ${percent}%"></div>
                </div>
                <span class="quota-text">${formatBytes(acc.quota.usage)} / ${formatBytes(acc.quota.limit)}</span>
            `;
        }

        return `
            <div class="account-card">
                <div class="account-header-row">
                    <div class="account-avatar" style="background-color: ${avatarBg}">${initial}</div>
                    <div class="account-email" title="${escapeHtml(acc.email)}">${escapeHtml(acc.email)}</div>
                </div>
                ${quotaHtml}
            </div>
        `;
    }).join('');

    let totalSummaryHtml = '';
    if (totalLimit > 0) {
        const totalPercent = Math.min(100, (totalUsage / totalLimit) * 100);
        totalSummaryHtml = `
            <div style="background: var(--accent-container); color: var(--accent-on-container); padding: 8px 10px; border-radius: var(--radius-md); margin-bottom: 6px;">
                <div style="display: flex; justify-content: space-between; font-size: 0.8rem; font-weight: 600; margin-bottom: 4px;">
                    <span>Total Storage</span>
                    <span>${formatBytes(totalUsage)} / ${formatBytes(totalLimit)}</span>
                </div>
                <div style="background: rgba(0,0,0,0.15); height: 5px; border-radius: 9999px; overflow: hidden;">
                    <div style="background: var(--accent-primary); height: 100%; width: ${totalPercent}%; border-radius: 9999px;"></div>
                </div>
            </div>
        `;
    }

    els.accountsList.innerHTML = totalSummaryHtml + cardsHtml;
}

async function renderSettingsAccounts() {
    const list = document.getElementById('settings-accounts-list');
    if (!list) return;
    if (state.accounts.length === 0) {
        list.innerHTML = `<div style="color: var(--text-tertiary); font-size: 0.86rem;">No accounts connected yet.</div>`;
        return;
    }

    // Fetch sync statuses for status badges
    let syncStatuses = [];
    try {
        syncStatuses = await invoke('cmd_get_sync_status');
    } catch (e) {}
    
    list.innerHTML = state.accounts.map(acc => {
        const initial = (acc.email[0] || 'U').toUpperCase();
        const avatarBg = getAvatarColor(acc.email);
        const quotaStr = acc.quota ? `${formatBytes(acc.quota.usage)} / ${formatBytes(acc.quota.limit)}` : '';
        const syncInfo = syncStatuses.find(s => s.account === acc.email);
        const syncStatus = syncInfo ? syncInfo.status : 'Idle';
        
        const needsReauth = syncStatus === 'NeedsReauth';
        const statusColor = needsReauth ? 'var(--accent-danger)' : 
                           syncStatus === 'Syncing' ? 'var(--accent-primary)' :
                           syncStatus === 'Offline' ? 'var(--text-tertiary)' : 'var(--accent-success)';
        const statusLabel = needsReauth ? '⚠ Re-auth needed' :
                           syncStatus === 'Syncing' ? '⟳ Syncing' :
                           syncStatus === 'Offline' ? '○ Offline' : '● Connected';
        
        const reconnectBtn = needsReauth
            ? `<button class="btn-secondary reconnect-acc-btn" data-email="${escapeHtml(acc.email)}" style="color: var(--accent-primary); border-color: var(--accent-primary); padding: 5px 12px; font-size: 0.8rem;">Reconnect</button>`
            : '';

        return `
            <div class="account-card" style="display: flex; justify-content: space-between; align-items: center; padding: 10px 14px;">
                <div style="display: flex; align-items: center; gap: 10px; min-width: 0;">
                    <div class="account-avatar" style="background-color: ${avatarBg}; width: 30px; height: 30px; font-size: 13px;">${initial}</div>
                    <div style="min-width: 0;">
                        <div style="font-weight: 500; font-size: 0.88rem; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">${escapeHtml(acc.email)}</div>
                        <div style="font-size: 0.76rem; color: var(--text-tertiary);">${quotaStr}</div>
                        <div style="font-size: 0.72rem; color: ${statusColor}; margin-top: 2px;">${statusLabel}</div>
                    </div>
                </div>
                <div style="display: flex; gap: 6px; align-items: center; flex-shrink: 0;">
                    ${reconnectBtn}
                    <button class="btn-secondary remove-acc-btn" data-email="${escapeHtml(acc.email)}" style="color: var(--accent-danger); border-color: var(--border-subtle); padding: 5px 12px; font-size: 0.8rem;">Remove</button>
                </div>
            </div>
        `;
    }).join('');
    
    list.querySelectorAll('.remove-acc-btn').forEach(btn => {
        btn.addEventListener('click', async (e) => {
            const email = e.target.dataset.email;
            if (await showConfirmModal('Remove Account', `Disconnect and remove account ${email}?`)) {
                try {
                    await invoke('cmd_remove_account', { email });
                    showToast('Account removed', 'success');
                    fetchAccounts();
                } catch (err) {
                    showToast(err, 'error');
                }
            }
        });
    });
    
    list.querySelectorAll('.reconnect-acc-btn').forEach(btn => {
        btn.addEventListener('click', async (e) => {
            const email = e.target.dataset.email;
            try {
                showToast('Opening browser for re-authentication...', 'info');
                await invoke('cmd_reconnect_account', { email });
                showToast(`Re-connected ${email}!`, 'success');
                renderSettingsAccounts();
            } catch (err) {
                showToast(`Reconnect failed: ${err}`, 'error');
            }
        });
    });
}

// ─── Photos & Media View ─────────────────────────────────────

async function loadMediaFiles() {
    if (!els.photosGrid) return;
    els.photosGrid.innerHTML = '';
    if (els.photosEmptyState) els.photosEmptyState.classList.add('hidden');
    if (els.photosLoadingState) els.photosLoadingState.classList.remove('hidden');
    
    try {
        const mediaFiles = await invoke('cmd_list_media_files');
        if (els.photosLoadingState) els.photosLoadingState.classList.add('hidden');
        if (els.photosCountBadge) {
            els.photosCountBadge.textContent = `${mediaFiles.length} item${mediaFiles.length === 1 ? '' : 's'}`;
        }
        
        if (mediaFiles.length === 0) {
            if (els.photosEmptyState) els.photosEmptyState.classList.remove('hidden');
            return;
        }
        
        renderMediaFiles(mediaFiles);
    } catch (err) {
        if (els.photosLoadingState) els.photosLoadingState.classList.add('hidden');
        showToast(`Failed to load media: ${err}`, 'error');
    }
}

function renderMediaFiles(files) {
    if (!els.photosGrid) return;
    els.photosGrid.innerHTML = '';
    
    files.forEach(file => {
        const card = document.createElement('div');
        card.className = 'photo-card';
        card.title = `${file.display_name || file.name}\n${formatBytes(file.size)}`;
        
        const isVideo = file.mime_type && file.mime_type.startsWith('video/');
        const accountBadge = file.account ? `<span class="photo-badge" style="background-color:${getAvatarColor(file.account)}">${escapeHtml(file.account.split('@')[0])}</span>` : '';
        const iconSvg = isVideo
            ? '<svg style="width:36px;height:36px;color:var(--text-tertiary)" viewBox="0 0 24 24" fill="currentColor"><path d="M17 10.5V7c0-.55-.45-1-1-1H4c-.55 0-1 .45-1 1v10c0 .55.45 1 1 1h12c.55 0 1-.45 1-1v-3.5l4 4v-11l-4 4z"/></svg>'
            : '<svg style="width:36px;height:36px;color:var(--text-tertiary)" viewBox="0 0 24 24" fill="currentColor"><path d="M21 19V5c0-1.1-.9-2-2-2H5c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h14c1.1 0 2-.9 2-2zM8.5 13.5l2.5 3.01L14.5 12l4.5 6H5l3.5-4.5z"/></svg>';
            
        const thumbUrl = file.thumbnail_link || '';
        const mediaVisual = thumbUrl
            ? `<img src="${thumbUrl}" alt="${escapeHtml(file.name)}" loading="lazy" style="width:100%;height:100%;object-fit:cover;" onerror="this.style.display='none';">`
            : iconSvg;

        card.innerHTML = `
            <div class="photo-thumbnail-container" id="thumb-${file.id}">
                ${mediaVisual}
                ${accountBadge}
            </div>
            <div class="photo-info">
                <div class="photo-name">${escapeHtml(file.display_name || file.name)}</div>
                <div class="photo-meta">
                    <span>${formatBytes(file.size)}</span>
                    <span>${formatRelativeTime(file.modified_time)}</span>
                </div>
            </div>
        `;
        
        // Click to open preview modal
        card.addEventListener('click', () => {
            openPreview(file);
        });
        
        // Right-click context menu
        card.addEventListener('contextmenu', (e) => {
            e.preventDefault();
            state.contextMenuTarget = file;
            els.contextMenu.style.left = `${Math.min(e.clientX, window.innerWidth - 210)}px`;
            els.contextMenu.style.top = `${Math.min(e.clientY, window.innerHeight - 240)}px`;
            els.contextMenu.classList.remove('hidden');
        });
        
        els.photosGrid.appendChild(card);
        
        // If no thumbnailLink from Google, fallback to lazy loading thumbnail bytes
        if (!thumbUrl && file.mime_type && file.mime_type.startsWith('image/') && file.size < 15 * 1024 * 1024) {
            lazyLoadThumbnail(file);
        }
    });
}

async function lazyLoadThumbnail(file) {
    try {
        const previewBytes = await invoke('cmd_get_file_preview', {
            nodeId: file.id,
            offset: 0,
            length: Math.min(file.size || 500000, 500000)
        });
        const blob = new Blob([new Uint8Array(previewBytes)], { type: file.mime_type });
        const url = URL.createObjectURL(blob);
        const thumbContainer = document.getElementById(`thumb-${file.id}`);
        if (thumbContainer) {
            const img = document.createElement('img');
            img.src = url;
            img.alt = file.name;
            img.onload = () => {
                const svg = thumbContainer.querySelector('svg');
                if (svg) svg.remove();
                thumbContainer.prepend(img);
            };
        }
    } catch (e) {
        // Fallback to placeholder icon
    }
}

function renderFiles() {
    if (state.files.length === 0) {
        els.emptyState.classList.remove('hidden');
        return;
    }
    
    const sorted = sortFiles(state.files, state.sortBy, state.sortDir);
    els.fileList.className = state.viewMode === 'grid' ? 'file-grid' : 'file-list-view';
    
    if (state.viewMode === 'grid') {
        els.fileList.innerHTML = sorted.map(file => {
            const iconData = getFileIcon(file.is_dir, file.mime_type);
            return `
                <div class="file-card" data-id="${file.id}" data-type="${file.is_dir ? 'dir' : 'file'}" role="button" tabindex="0">
                    <div class="file-icon-wrapper ${iconData.className}">
                        ${iconData.svg}
                    </div>
                    <div class="file-name" title="${escapeHtml(file.name)}">${escapeHtml(file.name)}</div>
                    <div class="file-meta">${file.is_dir ? 'Folder' : formatBytes(file.size)}</div>
                </div>
            `;
        }).join('');
    } else {
        const headerHtml = `
            <div class="file-list-header">
                <div></div>
                <div>Name</div>
                <div>Modified</div>
                <div>Size</div>
                <div></div>
            </div>
        `;
        const rowsHtml = sorted.map(file => {
            const iconData = getFileIcon(file.is_dir, file.mime_type);
            return `
                <div class="file-row" data-id="${file.id}" data-type="${file.is_dir ? 'dir' : 'file'}" role="button" tabindex="0">
                    <div class="file-icon-wrapper ${iconData.className}">
                        ${iconData.svg}
                    </div>
                    <div class="file-name" title="${escapeHtml(file.name)}">${escapeHtml(file.name)}</div>
                    <div class="file-meta">${formatRelativeTime(file.modified_time)}</div>
                    <div class="file-meta">${file.is_dir ? '--' : formatBytes(file.size)}</div>
                    <div style="color: var(--text-tertiary); text-align: right;">⋮</div>
                </div>
            `;
        }).join('');
        els.fileList.innerHTML = headerHtml + rowsHtml;
    }
    
    els.fileList.querySelectorAll('.file-card, .file-row').forEach(el => {
        el.addEventListener('click', (e) => handleFileClick(e, el.dataset.id));
        el.addEventListener('contextmenu', (e) => handleContextMenu(e, el.dataset.id));
        el.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') handleFileClick(e, el.dataset.id);
        });
    });
}

function renderBreadcrumbs() {
    els.breadcrumbs.innerHTML = state.currentPath.map((crumb, idx) => {
        const isLast = idx === state.currentPath.length - 1;
        const sep = isLast ? '' : `
            <span class="crumb-separator">
                <svg viewBox="0 0 24 24"><path d="M10 6L8.59 7.41 13.17 12l-4.58 4.59L10 18l6-6z"/></svg>
            </span>
        `;
        return `
            <span class="crumb" data-idx="${idx}">${escapeHtml(crumb.name)}</span>
            ${sep}
        `;
    }).join('');
    
    els.breadcrumbs.querySelectorAll('.crumb').forEach(el => {
        el.addEventListener('click', (e) => {
            const idx = parseInt(e.target.dataset.idx, 10);
            if (idx === state.currentPath.length - 1) return;
            state.currentPath = state.currentPath.slice(0, idx + 1);
            renderBreadcrumbs();
            loadFiles(getCurrentFolderId());
        });
    });
}

function renderTransfers() {
    const list = document.getElementById('transfers-list');
    if (!list) return;
    if (state.transfers.length === 0) {
        list.innerHTML = `
            <div class="empty-state" style="height: 30vh;">
                <div class="empty-icon">
                    <svg viewBox="0 0 24 24" fill="currentColor"><path d="M19.35 10.04C18.67 6.59 15.64 4 12 4 9.11 4 6.6 5.64 5.35 8.04 2.34 8.36 0 10.91 0 14c0 3.31 2.69 6 6 6h13c2.76 0 5-2.24 5-5 0-2.64-2.05-4.78-4.65-4.96zM14 13v4h-4v-4H7l5-5 5 5h-3z"/></svg>
                </div>
                <h3>No transfers</h3>
                <p>Upload files to see transfer progress here.</p>
            </div>
        `;
        return;
    }

    function getStatusClass(status) {
        if (status === 'Completed') return 'completed';
        if (status.startsWith('Failed')) return 'failed';
        if (status === 'Paused') return 'paused';
        if (status === 'Cancelled') return 'cancelled';
        return '';
    }

    function getStatusLabel(status) {
        if (status === 'InProgress') return 'Uploading';
        if (status === 'Pending') return 'Queued';
        if (status.startsWith('Failed:')) return 'Failed';
        return status;
    }

    const hasCompleted = state.transfers.some(t => 
        t.status === 'Completed' || t.status === 'Cancelled' || t.status.startsWith('Failed')
    );

    let headerHtml = '';
    if (hasCompleted) {
        headerHtml = `
            <div style="display: flex; justify-content: flex-end; margin-bottom: 8px;">
                <button id="clear-completed-btn" class="btn-secondary" style="font-size: 0.78rem; padding: 4px 12px;">
                    Clear Completed
                </button>
            </div>
        `;
    }
    
    list.innerHTML = headerHtml + state.transfers.map(t => {
        const percent = t.total_bytes > 0 ? (t.progress_bytes / t.total_bytes) * 100 : 0;
        const statusClass = getStatusClass(t.status);
        const statusLabel = getStatusLabel(t.status);
        return `
            <div class="transfer-item">
                <div class="transfer-header">
                    <span class="file-name" title="${escapeHtml(t.file_name)}">${escapeHtml(t.file_name)}</span>
                    <span class="status ${statusClass}">${escapeHtml(statusLabel)}</span>
                </div>
                <div class="transfer-progress-bg">
                    <div class="transfer-progress-fill" style="width: ${percent}%"></div>
                </div>
                <div style="display: flex; justify-content: space-between; align-items: center; font-size: 0.78rem; color: var(--text-tertiary);">
                    <span>${formatBytes(t.progress_bytes)} of ${formatBytes(t.total_bytes)}</span>
                    ${t.status === 'InProgress' || t.status === 'Pending' ? `
                        <button class="icon-btn cancel-transfer" data-id="${t.id}" title="Cancel" style="width: 28px; height: 28px;">
                            <svg viewBox="0 0 24 24"><path d="M19 6.41L17.59 5 12 10.59 6.41 5 5 6.41 10.59 12 5 17.59 6.41 19 12 13.41 17.59 19 19 17.59 13.41 12z"/></svg>
                        </button>` : ''}
                </div>
            </div>
        `;
    }).join('');
    
    list.querySelectorAll('.cancel-transfer').forEach(btn => {
        btn.addEventListener('click', async (e) => {
            try {
                await invoke('cmd_cancel_transfer', { id: e.target.closest('[data-id]').dataset.id });
                updateTransfers();
            } catch (err) {
                showToast(err, 'error');
            }
        });
    });

    const clearBtn = list.querySelector('#clear-completed-btn');
    if (clearBtn) {
        clearBtn.addEventListener('click', () => {
            state.transfers = state.transfers.filter(t => 
                t.status !== 'Completed' && t.status !== 'Cancelled' && !t.status.startsWith('Failed')
            );
            renderTransfers();
            showToast('Cleared completed transfers', 'success');
        });
    }
}

async function handleFileClick(e, id) {
    const file = state.files.find(f => f.id === id);
    if (!file) return;
    
    if (file.is_dir) {
        state.currentPath.push({ id: file.id, name: file.name });
        renderBreadcrumbs();
        loadFiles(file.id);
    } else {
        openPreview(file);
    }
}

function handleContextMenu(e, id) {
    e.preventDefault();
    state.contextMenuTarget = state.files.find(f => f.id === id);
    if (!state.contextMenuTarget) return;
    
    els.contextMenu.style.left = `${Math.min(e.clientX, window.innerWidth - 210)}px`;
    els.contextMenu.style.top = `${Math.min(e.clientY, window.innerHeight - 240)}px`;
    els.contextMenu.classList.remove('hidden');
}

async function handleContextAction(action) {
    const file = state.contextMenuTarget;
    if (!file) return;
    
    try {
        switch (action) {
            case 'open':
                if (file.is_dir) {
                    state.currentPath.push({ id: file.id, name: file.name });
                    renderBreadcrumbs();
                    loadFiles(file.id);
                } else {
                    openPreview(file);
                }
                break;
            case 'preview':
                if (!file.is_dir) openPreview(file);
                break;
            case 'rename':
                const newName = await showPromptModal('Rename', 'Enter a new name:', file.name);
                if (newName && newName.trim() && newName.trim() !== file.name) {
                    await invoke('cmd_rename_file', {
                        nodeId: file.id,
                        newName: newName.trim()
                    });
                    loadFiles(getCurrentFolderId());
                    showToast('File renamed', 'success');
                }
                break;
            case 'trash':
                if (await showConfirmModal('Move to Trash', `Move "${file.name}" to trash?`)) {
                    await invoke('cmd_trash_file', { nodeId: file.id });
                    loadFiles(getCurrentFolderId());
                    showToast('Moved to trash', 'info');
                }
                break;
            case 'delete':
                if (await showConfirmModal('Delete Permanently', `Permanently delete "${file.name}"? This cannot be undone.`, 'Delete', true)) {
                    await invoke('cmd_delete_permanently', { nodeId: file.id });
                    loadFiles(getCurrentFolderId());
                    showToast('File permanently deleted', 'info');
                }
                break;
            case 'download':
                try {
                    showToast('Preparing download...', 'info');
                    const savePath = await window.__TAURI__.dialog.save({
                        defaultPath: file.display_name || file.name,
                        title: 'Save file as',
                    });
                    if (!savePath) return;
                    await invoke('cmd_download_to_file', {
                        nodeId: file.id,
                        destPath: savePath,
                    });
                    showToast('Download complete!', 'success');
                } catch (dlErr) {
                    showToast(`Download failed: ${dlErr}`, 'error');
                }
                break;
        }
    } catch (err) {
        showToast(err, 'error');
    }
}

async function openPreview(file) {
    els.previewTitle.textContent = file.name;
    els.previewBody.innerHTML = '<div class="spinner"></div>';
    els.previewModal.classList.remove('hidden');
    
    try {
        const data = await invoke('cmd_get_file_preview', {
            nodeId: file.id,
            offset: 0,
            length: Math.min(file.size, 5 * 1024 * 1024)
        });
        
        const blob = new Blob([new Uint8Array(data)], { type: file.mime_type });
        const url = URL.createObjectURL(blob);
        
        if (file.mime_type.startsWith('image/')) {
            els.previewBody.innerHTML = `<img src="${url}" alt="preview">`;
        } else if (file.mime_type.startsWith('video/')) {
            els.previewBody.innerHTML = `<video controls autoplay><source src="${url}" type="${file.mime_type}"></video>`;
        } else if (file.mime_type === 'application/pdf') {
            els.previewBody.innerHTML = `<iframe src="${url}" style="width:100%;height:100%;border:none;border-radius:var(--radius-sm);"></iframe>`;
        } else if (file.mime_type.startsWith('text/') || file.mime_type.includes('json') || file.mime_type.includes('xml')) {
            const text = new TextDecoder().decode(new Uint8Array(data));
            els.previewBody.innerHTML = `<pre>${escapeHtml(text)}</pre>`;
        } else {
            els.previewBody.innerHTML = `
                <div style="text-align:center; color:var(--text-tertiary);">
                    <div style="font-size:3rem; margin-bottom:12px;">📄</div>
                    <p>No preview available for this file type.</p>
                </div>
            `;
        }

        // Wire up the download button in preview modal
        const downloadBtn = document.getElementById('download-preview-btn');
        if (downloadBtn) {
            downloadBtn.onclick = async () => {
                try {
                    showToast('Preparing download...', 'info');
                    const savePath = await window.__TAURI__.dialog.save({
                        defaultPath: file.display_name || file.name,
                        title: 'Save file as',
                    });
                    if (!savePath) return;
                    await invoke('cmd_download_to_file', {
                        nodeId: file.id,
                        destPath: savePath,
                    });
                    showToast('Download complete!', 'success');
                } catch (dlErr) {
                    showToast(`Download failed: ${dlErr}`, 'error');
                }
            };
        }
    } catch (err) {
        els.previewBody.innerHTML = `<p style="color:var(--accent-danger)">Error loading preview: ${escapeHtml(String(err))}</p>`;
    }
}

function setupEventListeners() {
    document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') {
            els.contextMenu.classList.add('hidden');
            document.querySelectorAll('.modal-overlay:not(.hidden)').forEach(m => {
                if (m.id !== 'setup-wizard') m.classList.add('hidden');
            });
        }
    });

    document.addEventListener('click', (e) => {
        if (!els.contextMenu.contains(e.target)) {
            els.contextMenu.classList.add('hidden');
        }
    });

    els.contextMenu.querySelectorAll('li[data-action]').forEach(el => {
        el.addEventListener('click', () => {
            handleContextAction(el.dataset.action);
            els.contextMenu.classList.add('hidden');
        });
    });

    document.querySelectorAll('.modal-overlay').forEach(overlay => {
        overlay.addEventListener('click', (e) => {
            if (e.target === overlay) overlay.classList.add('hidden');
        });
        overlay.querySelectorAll('.close-modal').forEach(btn => {
            btn.addEventListener('click', () => overlay.classList.add('hidden'));
        });
    });

    let searchTimeout;
    els.searchInput.addEventListener('input', (e) => {
        const val = e.target.value.trim();
        els.clearSearchBtn.classList.toggle('hidden', val.length === 0);
        
        clearTimeout(searchTimeout);
        searchTimeout = setTimeout(() => {
            loadFiles(getCurrentFolderId(), val.length > 0 ? val : null);
        }, 300);
    });

    els.clearSearchBtn.addEventListener('click', () => {
        els.searchInput.value = '';
        els.clearSearchBtn.classList.add('hidden');
        loadFiles(getCurrentFolderId());
    });

    els.saveCredsBtn.addEventListener('click', async () => {
        const clientId = els.clientIdInput.value.trim();
        const clientSecret = els.clientSecretInput.value.trim();
        if (!clientId || !clientSecret) {
            els.setupError.textContent = 'Both fields are required.';
            els.setupError.classList.remove('hidden');
            return;
        }
        
        try {
            await invoke('cmd_set_client_credentials', { clientId, clientSecret });
            els.setupWizard.classList.add('hidden');
            showToast('Google API Credentials saved!', 'success');
            await loadInitialData();
        } catch (err) {
            els.setupError.textContent = err;
            els.setupError.classList.remove('hidden');
        }
    });

    const saveSettingsCredsBtn = document.getElementById('save-settings-creds-btn');
    if (saveSettingsCredsBtn) {
        saveSettingsCredsBtn.addEventListener('click', async () => {
            const clientId = document.getElementById('settings-client-id').value.trim();
            const clientSecret = document.getElementById('settings-client-secret').value.trim();
            if (!clientId || !clientSecret) {
                showToast('Please enter both Client ID and Client Secret', 'error');
                return;
            }
            try {
                await invoke('cmd_set_client_credentials', { clientId, clientSecret });
                showToast('Google Credentials updated successfully!', 'success');
                fetchSettings();
            } catch (err) {
                showToast(`Failed to update credentials: ${err}`, 'error');
            }
        });
    }

    document.getElementById('add-account-sidebar-btn').addEventListener('click', () => triggerAddAccount());
    document.getElementById('add-account-btn').addEventListener('click', () => {
        const hint = document.getElementById('new-account-email').value;
        triggerAddAccount(hint);
    });

    els.toggleViewBtn.addEventListener('click', () => {
        state.viewMode = state.viewMode === 'grid' ? 'list' : 'grid';
        renderFiles();
    });

    const sortSelect = document.getElementById('sort-select');
    if (sortSelect) {
        sortSelect.addEventListener('change', (e) => {
            const [by, dir] = e.target.value.split('-');
            state.sortBy = by;
            state.sortDir = dir;
            renderFiles();
        });
    }

    document.getElementById('new-folder-btn').addEventListener('click', () => {
        els.newFolderName.value = '';
        els.newFolderModal.classList.remove('hidden');
        els.newFolderName.focus();
    });

    document.getElementById('create-folder-confirm-btn').addEventListener('click', async () => {
        const name = els.newFolderName.value.trim();
        if (!name) return;
        
        try {
            await invoke('cmd_create_folder', {
                parentId: getCurrentFolderId(),
                name
            });
            els.newFolderModal.classList.add('hidden');
            loadFiles(getCurrentFolderId());
            showToast('Folder created', 'success');
        } catch (err) {
            showToast(err, 'error');
        }
    });
    
    const syncBtn = document.getElementById('sync-btn');
    if (syncBtn) {
        syncBtn.addEventListener('click', withLoading(syncBtn, async () => {
            try {
                showToast('Syncing with Google Drive...');
                await invoke('cmd_force_sync');
                showToast('Drive is up to date', 'success');
                loadFiles(getCurrentFolderId());
                if (state.currentView === 'photos') {
                    loadMediaFiles();
                }
                await fetchAccounts();
            } catch (err) {
                showToast(`Sync failed: ${err}`, 'error');
            }
        }));
    }

    // Upload button — opens native file picker and uploads selected files
    const uploadBtn = document.getElementById('upload-btn');
    if (uploadBtn) {
        uploadBtn.addEventListener('click', async () => {
            try {
                const selected = await window.__TAURI__.dialog.open({
                    multiple: true,
                    title: 'Select files to upload',
                });
                if (!selected) return; // user cancelled
                
                const paths = Array.isArray(selected) ? selected : [selected];
                const parentId = getCurrentFolderId();
                
                for (const filePath of paths) {
                    try {
                        await invoke('cmd_upload_file', { localPath: filePath, parentId });
                        showToast(`Uploading: ${filePath.split('/').pop()}`, 'info');
                    } catch (upErr) {
                        showToast(`Upload failed: ${upErr}`, 'error');
                    }
                }
                
                // Switch to transfers view to show progress
                window.location.hash = '#transfers';
                updateTransfers();
            } catch (err) {
                showToast(`Upload error: ${err}`, 'error');
            }
        });
    }

    document.getElementById('setting-cache').addEventListener('input', (e) => {
        document.getElementById('setting-cache-val').textContent = formatBytes(parseInt(e.target.value, 10));
    });
    
    document.getElementById('save-settings-btn').addEventListener('click', async () => {
        if (!state.settings) return;
        
        const settings = {
            ...state.settings,
            cache_size_bytes: parseInt(document.getElementById('setting-cache').value, 10),
            sync_interval_secs: parseInt(document.getElementById('setting-sync').value, 10),
            upload_policy: document.getElementById('setting-upload-policy').value
        };
        
        try {
            await invoke('cmd_update_settings', { settings });
            
            const theme = document.getElementById('setting-theme').value;
            localStorage.setItem('theme', theme);
            applyTheme();
            
            showToast('Settings saved successfully', 'success');
        } catch (err) {
            showToast(err, 'error');
        }
    });

    // Legal page modal handlers
    const privacyBtn = document.getElementById('about-privacy-btn');
    if (privacyBtn) {
        privacyBtn.addEventListener('click', () => {
            document.getElementById('privacy-modal').classList.remove('hidden');
        });
    }

    const termsBtn = document.getElementById('about-terms-btn');
    if (termsBtn) {
        termsBtn.addEventListener('click', () => {
            document.getElementById('terms-modal').classList.remove('hidden');
        });
    }

    const licensesBtn = document.getElementById('about-licenses-btn');
    if (licensesBtn) {
        licensesBtn.addEventListener('click', () => {
            document.getElementById('licenses-modal').classList.remove('hidden');
        });
    }

    // Load app version dynamically
    loadAppVersion();
}

async function loadAppVersion() {
    try {
        const version = await invoke('cmd_get_app_version');
        const versionEl = document.getElementById('app-version');
        if (versionEl) {
            versionEl.textContent = `Version ${version}`;
        }
    } catch (e) {
        // Fallback: keep the static version text
    }
}

// File sorting
function sortFiles(files, sortBy = 'name', sortDir = 'asc') {
    return [...files].sort((a, b) => {
        // Folders always first
        if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;

        let cmp = 0;
        switch (sortBy) {
            case 'name':
                cmp = (a.name || '').localeCompare(b.name || '', undefined, { sensitivity: 'base' });
                break;
            case 'date':
                cmp = new Date(a.modified_time || 0) - new Date(b.modified_time || 0);
                break;
            case 'size':
                cmp = (a.size || 0) - (b.size || 0);
                break;
            default:
                cmp = 0;
        }
        return sortDir === 'desc' ? -cmp : cmp;
    });
}

function setupDragAndDrop() {
    window.addEventListener('dragover', (e) => {
        e.preventDefault();
        els.dropOverlay.classList.remove('hidden');
    });

    els.dropOverlay.addEventListener('dragleave', (e) => {
        if (e.target === els.dropOverlay) {
            els.dropOverlay.classList.add('hidden');
        }
    });

    els.dropOverlay.addEventListener('drop', async (e) => {
        e.preventDefault();
        els.dropOverlay.classList.add('hidden');
        if (state.currentView !== 'pool') {
            showToast('Switch to Unified Pool view to upload files.', 'info');
            return;
        }
        const files = e.dataTransfer.files;
        if (!files || files.length === 0) return;
        const parentId = getCurrentFolderId();
        let count = 0;
        for (const file of files) {
            if (file.path) {
                try {
                    await invoke('cmd_upload_file', { localPath: file.path, parentId });
                    count++;
                } catch (upErr) {
                    showToast(`Upload failed: ${file.name} — ${upErr}`, 'error');
                }
            }
        }
        if (count > 0) {
            showToast(`${count} file(s) queued for upload`, 'success');
            window.location.hash = '#transfers';
            updateTransfers();
        }
    });
}

async function triggerAddAccount(hint = '') {
    try {
        const hasCreds = await invoke('cmd_has_client_credentials');
        if (!hasCreds) {
            showToast('Please configure Google API Credentials first', 'error');
            els.setupWizard.classList.remove('hidden');
            return;
        }

        showToast('Opening default browser for Google sign-in...', 'info');
        const emailHint = hint && hint.trim() ? hint.trim() : null;
        const email = await invoke('cmd_add_account', { emailHint });
        showToast(`Successfully connected ${email}!`, 'success');
        await fetchAccounts();
        loadFiles(getCurrentFolderId());
    } catch (err) {
        showToast(err, 'error');
    }
}

function getCurrentFolderId() {
    return state.currentPath[state.currentPath.length - 1].id;
}

function applyTheme() {
    const pref = localStorage.getItem('theme') || 'dark';
    const isDark = pref === 'dark' || (pref === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches);
    
    document.documentElement.classList.toggle('light-theme', !isDark);
    const themeSelect = document.getElementById('setting-theme');
    if (themeSelect) themeSelect.value = pref;
}

function formatBytes(bytes) {
    if (bytes === 0 || !bytes) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
}

function formatRelativeTime(isoString) {
    if (!isoString) return '--';
    const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
    const date = new Date(isoString);
    const diff = (date.getTime() - Date.now()) / 1000;
    
    if (Math.abs(diff) < 60) return rtf.format(Math.round(diff), 'second');
    if (Math.abs(diff) < 3600) return rtf.format(Math.round(diff / 60), 'minute');
    if (Math.abs(diff) < 86400) return rtf.format(Math.round(diff / 3600), 'hour');
    return rtf.format(Math.round(diff / 86400), 'day');
}

function getFileIcon(isDir, mimeType = '') {
    if (isDir) {
        return {
            className: 'icon-folder',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M10 4H4c-1.1 0-1.99.9-1.99 2L2 18c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>'
        };
    }
    if (mimeType.startsWith('image/')) {
        return {
            className: 'icon-image',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M21 19V5c0-1.1-.9-2-2-2H5c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h14c1.1 0 2-.9 2-2zM8.5 13.5l2.5 3.01L14.5 12l4.5 6H5l3.5-4.5z"/></svg>'
        };
    }
    if (mimeType.startsWith('video/')) {
        return {
            className: 'icon-video',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M17 10.5V7c0-.55-.45-1-1-1H4c-.55 0-1 .45-1 1v10c0 .55.45 1 1 1h12c.55 0 1-.45 1-1v-3.5l4 4v-11l-4 4z"/></svg>'
        };
    }
    if (mimeType.startsWith('audio/')) {
        return {
            className: 'icon-audio',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M12 3v10.55c-.59-.34-1.27-.55-2-.55-2.21 0-4 1.79-4 4s1.79 4 4 4 4-1.79 4-4V7h4V3h-6z"/></svg>'
        };
    }
    if (mimeType === 'application/pdf') {
        return {
            className: 'icon-pdf',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M20 2H8c-1.1 0-2 .9-2 2v12c0 1.1.9 2 2 2h12c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2zm-8.5 7.5c0 .83-.67 1.5-1.5 1.5H9v2H7.5V7H10c.83 0 1.5.67 1.5 1.5v1zm5 2c0 .83-.67 1.5-1.5 1.5h-2.5V7H15c.83 0 1.5.67 1.5 1.5v3zm4-3H19v1h1.5V11H19v2h-1.5V7h3v1.5z"/></svg>'
        };
    }
    if (mimeType.includes('spreadsheet') || mimeType.includes('excel') || mimeType.includes('csv')) {
        return {
            className: 'icon-sheet',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M19 3H5c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h14c1.1 0 2-.9 2-2V5c0-1.1-.9-2-2-2zM9 17H6v-2h3v2zm0-4H6v-2h3v2zm0-4H6V7h3v2zm5 8h-3v-2h3v2zm0-4h-3v-2h3v2zm0-4h-3V7h3v2zm5 8h-3v-2h3v2zm0-4h-3v-2h3v2zm0-4h-3V7h3v2z"/></svg>'
        };
    }
    if (mimeType.includes('document') || mimeType.includes('word') || mimeType.startsWith('text/')) {
        return {
            className: 'icon-doc',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M14 2H6c-1.1 0-1.99.9-1.99 2L4 20c0 1.1.89 2 1.99 2H18c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>'
        };
    }
    if (mimeType.includes('zip') || mimeType.includes('tar') || mimeType.includes('rar')) {
        return {
            className: 'icon-archive',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M20 6h-8l-2-2H4c-1.1 0-1.99.9-1.99 2L2 18c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2zm-6 10h-4v-2h4v2zm0-4h-4v-2h4v2z"/></svg>'
        };
    }
    if (mimeType.includes('json') || mimeType.includes('xml') || mimeType.includes('javascript') || mimeType.includes('rust')) {
        return {
            className: 'icon-code',
            svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M9.4 16.6L4.8 12l4.6-4.6L8 6l-6 6 6 6 1.4-1.4zm5.2 0l4.6-4.6-4.6-4.6L16 6l6 6-6 6-1.4-1.4z"/></svg>'
        };
    }
    return {
        className: 'icon-default',
        svg: '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M6 2c-1.1 0-1.99.9-1.99 2L4 20c0 1.1.89 2 1.99 2H18c1.1 0 2-.9 2-2V8l-6-6H6zm7 7V3.5L18.5 9H13z"/></svg>'
    };
}

function escapeHtml(unsafe) {
    if (!unsafe) return '';
    return String(unsafe)
         .replace(/&/g, "&amp;")
         .replace(/</g, "&lt;")
         .replace(/>/g, "&gt;")
         .replace(/"/g, "&quot;")
         .replace(/'/g, "&#039;");
}

function showToast(message, type = 'info') {
    const toast = document.createElement('div');
    toast.className = `toast ${type}`;
    toast.textContent = message;
    
    els.toastContainer.appendChild(toast);
    
    setTimeout(() => toast.classList.add('show'), 10);
    
    setTimeout(() => {
        toast.classList.remove('show');
        setTimeout(() => toast.remove(), 250);
    }, 3500);
}

function showPromptModal(title, message, defaultValue = '') {
    return new Promise((resolve) => {
        const overlay = document.createElement('div');
        overlay.className = 'modal-overlay';
        overlay.innerHTML = `
            <div class="modal-content">
                <h2>${escapeHtml(title)}</h2>
                <p>${escapeHtml(message)}</p>
                <div class="form-group">
                    <input type="text" id="prompt-modal-input" value="${escapeHtml(defaultValue)}" autofocus>
                </div>
                <div class="form-actions">
                    <button class="btn-secondary" id="prompt-cancel">Cancel</button>
                    <button class="btn-primary" id="prompt-confirm">OK</button>
                </div>
            </div>
        `;
        document.body.appendChild(overlay);
        const input = overlay.querySelector('#prompt-modal-input');
        input.focus();
        input.select();

        const cleanup = (result) => {
            overlay.classList.add('hidden');
            setTimeout(() => overlay.remove(), 200);
            resolve(result);
        };

        overlay.querySelector('#prompt-cancel').addEventListener('click', () => cleanup(null));
        overlay.querySelector('#prompt-confirm').addEventListener('click', () => cleanup(input.value));
        overlay.addEventListener('click', (e) => { if (e.target === overlay) cleanup(null); });
        input.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') cleanup(input.value);
            if (e.key === 'Escape') cleanup(null);
        });
    });
}

function showConfirmModal(title, message, confirmText = 'Confirm', isDanger = false) {
    return new Promise((resolve) => {
        const overlay = document.createElement('div');
        overlay.className = 'modal-overlay';
        const btnClass = isDanger ? 'btn-danger' : 'btn-primary';
        overlay.innerHTML = `
            <div class="modal-content">
                <h2>${escapeHtml(title)}</h2>
                <p>${escapeHtml(message)}</p>
                <div class="form-actions">
                    <button class="btn-secondary" id="confirm-cancel">Cancel</button>
                    <button class="${btnClass}" id="confirm-ok">${escapeHtml(confirmText)}</button>
                </div>
            </div>
        `;
        document.body.appendChild(overlay);

        const cleanup = (result) => {
            overlay.classList.add('hidden');
            setTimeout(() => overlay.remove(), 200);
            resolve(result);
        };

        overlay.querySelector('#confirm-cancel').addEventListener('click', () => cleanup(false));
        overlay.querySelector('#confirm-ok').addEventListener('click', () => cleanup(true));
        overlay.addEventListener('click', (e) => { if (e.target === overlay) cleanup(false); });
        document.addEventListener('keydown', function handler(e) {
            if (e.key === 'Escape') { cleanup(false); document.removeEventListener('keydown', handler); }
        });
    });
}

function withLoading(btn, asyncFn) {
    return async (...args) => {
        if (btn.disabled) return;
        const originalHtml = btn.innerHTML;
        btn.disabled = true;
        btn.style.opacity = '0.7';
        btn.style.pointerEvents = 'none';
        try {
            await asyncFn(...args);
        } finally {
            btn.disabled = false;
            btn.style.opacity = '';
            btn.style.pointerEvents = '';
            btn.innerHTML = originalHtml;
        }
    };
}

function formatDate(isoString) {
    if (!isoString) return '--';
    return new Date(isoString).toLocaleDateString(undefined, {
        year: 'numeric', month: 'short', day: 'numeric'
    });
}

document.addEventListener('DOMContentLoaded', init);
