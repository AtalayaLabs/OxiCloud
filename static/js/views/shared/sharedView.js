/**
 * OxiCloud - Shared View Component
 * In-app shared files view. All operations go through the backend API.
 */

import { switchToFilesSection } from '../../app/navigation.js';
import { ui } from '../../app/ui.js';
import { getCsrfHeaders } from '../../core/csrf.js';
import { formatDateShort } from '../../core/formatters.js';
import { i18n } from '../../core/i18n.js';
import { fileSharing } from '../../features/sharing/fileSharing.js';

/** @import {Share} from '../../core/types.js' */

const TTL = 5 * 60 * 1000; // 5 min

const sharedView = {
    // State

    /** @type {Array<Share>} */
    items: [],

    _expires: 0,

    /** @type {Map<string, boolean>} key = "file:<id>" | "folder:<id>" */
    _knownItemsId: new Map(),

    /** @type {Array<Share>} */
    filteredItems: [],
    currentItem: null,

    /** Auth header helper — tokens are in HttpOnly cookies now */
    _headers(json = false) {
        const h = { ...getCsrfHeaders() };
        if (json) h['Content-Type'] = 'application/json';
        return h;
    },

    async init() {
        console.log('Initializing shared view component (API-backed)');
        await this.loadItems();
    },

    show() {
        this.displayUI();
        this.attachEventListeners();
        this.filterAndSortItems();
        const c = document.getElementById('shared-container');
        if (c) c.classList.remove('hidden');
    },

    hide() {
        const c = document.getElementById('shared-container');
        if (c) c.classList.add('hidden');
    },

    /**
     * tells if item_id is shared
     *
     * @param {string} id the item_id
     * @param {string} type folder|file
     * @returns {boolean} true if this item is shared
     */
    isShared(id, type) {
        return this._knownItemsId.has(`${type}:${id}`);
    },

    // Load shared items from backend API,
    // TODO cache entries to minimize calls
    /**
     * load shared items
     *
     * @param {boolean} force ignore cache
     */
    async loadItems(force = false) {
        if (this._expires > Date.now() && !force) return;

        try {
            const res = await fetch('/api/shares?page=1&per_page=1000', {
                headers: this._headers()
            });
            if (res.ok) {
                const data = await res.json();
                this.items = data.items || [];
            } else {
                this.items = [];
            }
            this.filteredItems = { ...this.items };
            this._knownItemsId.clear();
            this.items.forEach((item) => {
                this._knownItemsId.set(`${item.item_type}:${item.item_id}`, true);
            });
            this._expires = Date.now() + TTL;
        } catch (err) {
            console.error('Error loading shared items:', err);
            this.items = [];
        }
    },

    // Create and display the shared view UI
    displayUI() {
        const contentArea = document.querySelector('.content-area');

        let container = document.getElementById('shared-container');
        if (!container) {
            container = document.createElement('div');
            container.id = 'shared-container';
            container.className = 'shared-view-container';
            if (contentArea) contentArea.appendChild(container);
        }

        container.innerHTML = `
            <div class="shared-header">
                <div class="shared-filters">
                    <div class="shared-custom-select" id="filter-type-wrapper">
                        <button class="shared-select-toggle" id="filter-type-toggle">
                            <span class="shared-select-label" data-i18n="shared_filterAll">All</span>
                            <i class="fas fa-chevron-down shared-select-arrow"></i>
                        </button>
                        <div class="shared-select-dropdown" id="filter-type-dropdown">
                            <div class="shared-select-option active" data-value="all" data-i18n="shared_filterAll">All</div>
                            <div class="shared-select-option" data-value="file" data-i18n="shared_filterFiles">Files</div>
                            <div class="shared-select-option" data-value="folder" data-i18n="shared_filterFolders">Folders</div>
                        </div>
                    </div>
                    <div class="shared-custom-select" id="sort-by-wrapper">
                        <button class="shared-select-toggle" id="sort-by-toggle">
                            <span class="shared-select-label" data-i18n="shared_sortByDate">Sort by date</span>
                            <i class="fas fa-chevron-down shared-select-arrow"></i>
                        </button>
                        <div class="shared-select-dropdown" id="sort-by-dropdown">
                            <div class="shared-select-option active" data-value="date" data-i18n="shared_sortByDate">Sort by date</div>
                            <div class="shared-select-option" data-value="name" data-i18n="shared_sortByName">Sort by name</div>
                            <div class="shared-select-option" data-value="expiration" data-i18n="shared_sortByExpiration">Sort by expiration</div>
                        </div>
                    </div>
                </div>
            </div>

            <div id="empty-shared-state" class="empty-state hidden">
                <i class="fas fa-share-alt empty-state-icon"></i>
                <p data-i18n="shared_emptyStateTitle">No shared items</p>
                <p data-i18n="shared_emptyStateDesc">Items you share will appear here</p>
                <button id="go-to-files-btn" class="button primary" data-i18n="shared.goToFiles">Go to Files</button>
            </div>

            <div class="shared-list-container hidden">
                <table class="shared-table">
                    <thead>
                        <tr>
                            <th data-i18n="shared_colName">Name</th>
                            <th data-i18n="shared_colType">Type</th>
                            <th data-i18n="shared_colDateShared">Date</th>
                            <th data-i18n="shared_colExpiration">Expiration</th>
                            <th data-i18n="shared_colPermissions">Permissions</th>
                            <th data-i18n="shared_colPassword">Password</th>
                            <th data-i18n="shared_colActions">Actions</th>
                        </tr>
                    </thead>
                    <tbody id="shared-items-list"></tbody>
                </table>
            </div>

            <!-- Share Edit Dialog (sharedView-specific) -->
            <div id="shared-view-edit-dialog" class="shared-dialog hidden">
                <div class="shared-dialog-content">
                    <div class="shared-dialog-header">
                        <span id="sv-dialog-icon">📄</span>
                        <span id="sv-dialog-name">Item</span>
                        <button class="close-dialog-btn">&times;</button>
                    </div>
                    <div class="share-link-section">
                        <label data-i18n="share.linkLabel">Share Link:</label>
                        <div class="share-link-input">
                            <input type="text" id="sv-share-link-url" readonly>
                            <button id="sv-copy-link-btn" class="button" data-i18n="share.copyLink">Copy</button>
                        </div>
                    </div>
                    <div class="share-permissions-section">
                        <h4 data-i18n="share.permissions">Permissions</h4>
                        <label><input type="checkbox" id="sv-permission-read" checked> <span data-i18n="share.permissionRead">Read</span></label>
                        <label><input type="checkbox" id="sv-permission-write"> <span data-i18n="share.permissionWrite">Write</span></label>
                        <label><input type="checkbox" id="sv-permission-reshare"> <span data-i18n="share.permissionReshare">Reshare</span></label>
                    </div>
                    <div class="share-password-section">
                        <label><input type="checkbox" id="sv-enable-password"> <span data-i18n="share.password">Password protection</span></label>
                        <div class="password-input-group">
                            <input type="text" id="sv-share-password" disabled placeholder="Enter password">
                            <button id="sv-generate-password" class="button small" data-i18n="share.generatePassword">Generate</button>
                        </div>
                    </div>
                    <div class="share-expiration-section">
                        <label><input type="checkbox" id="sv-enable-expiration"> <span data-i18n="share.expiration">Set expiration</span></label>
                        <input type="date" id="sv-share-expiration" disabled>
                    </div>
                    <div class="share-actions">
                        <button id="sv-update-share-btn" class="button primary" data-i18n="share.update">Update</button>
                        <button id="sv-remove-share-btn" class="button danger" data-i18n="share.remove">Remove Share</button>
                    </div>
                </div>
            </div>

            <!-- Notification Dialog (sharedView-specific) -->
            <div id="sv-notification-dialog" class="shared-dialog hidden">
                <div class="shared-dialog-content">
                    <div class="shared-dialog-header">
                        <span id="sv-notify-dialog-icon">📧</span>
                        <span id="sv-notify-dialog-name">Item</span>
                        <button class="close-dialog-btn">&times;</button>
                    </div>
                    <div class="notification-form">
                        <div class="form-group">
                            <label data-i18n="share.notifyEmailLabel">Email:</label>
                            <input type="email" id="sv-notification-email" placeholder="recipient@example.com">
                        </div>
                        <div class="form-group">
                            <label data-i18n="share.notifyMessageLabel">Message (optional):</label>
                            <textarea id="sv-notification-message" rows="3"></textarea>
                        </div>
                    </div>
                    <div class="notification-actions">
                        <button id="sv-send-notification-btn" class="button primary" data-i18n="share.notifySend">Send Notification</button>
                    </div>
                </div>
            </div>
        `;

        i18n.translateElement(container);
    },

    // Attach event listeners
    attachEventListeners() {
        // Custom dropdown logic for filter-type
        this._initCustomSelect('filter-type-wrapper', 'filter-type-toggle', 'filter-type-dropdown');
        // Custom dropdown logic for sort-by
        this._initCustomSelect('sort-by-wrapper', 'sort-by-toggle', 'sort-by-dropdown');

        // Close dropdowns when clicking outside
        document.addEventListener('click', (e) => {
            document.querySelectorAll('.shared-custom-select.open').forEach((sel) => {
                if (!sel.contains(e.target)) sel.classList.remove('open');
            });
        });

        // Share dialog (sharedView-specific IDs)
        const shareDialog = document.getElementById('shared-view-edit-dialog');
        if (shareDialog) {
            const closeBtn = shareDialog.querySelector('.close-dialog-btn');
            if (closeBtn) closeBtn.addEventListener('click', () => this.closeShareDialog());
            const copyLinkBtn = document.getElementById('sv-copy-link-btn');
            if (copyLinkBtn) copyLinkBtn.addEventListener('click', () => this.copyShareLink());
            const enablePw = document.getElementById('sv-enable-password');
            const pwField = document.getElementById('sv-share-password');
            if (enablePw)
                enablePw.addEventListener('change', () => {
                    if (pwField) {
                        pwField.disabled = !enablePw.checked;
                        if (enablePw.checked) pwField.focus();
                    }
                });
            const genPwBtn = document.getElementById('sv-generate-password');
            if (genPwBtn) genPwBtn.addEventListener('click', () => this.generatePassword());
            const enableExp = document.getElementById('sv-enable-expiration');
            const expField = document.getElementById('sv-share-expiration');
            if (enableExp)
                enableExp.addEventListener('change', () => {
                    if (expField) {
                        expField.disabled = !enableExp.checked;
                        if (enableExp.checked) expField.focus();
                    }
                });
            const updateBtn = document.getElementById('sv-update-share-btn');
            if (updateBtn) updateBtn.addEventListener('click', () => this.updateSharedItem());
            const removeBtn = document.getElementById('sv-remove-share-btn');
            if (removeBtn) removeBtn.addEventListener('click', () => this.removeSharedItem());
        }

        // Notification dialog (sharedView-specific IDs)
        const notifDialog = document.getElementById('sv-notification-dialog');
        if (notifDialog) {
            const closeBtn = notifDialog.querySelector('.close-dialog-btn');
            if (closeBtn) closeBtn.addEventListener('click', () => this.closeNotificationDialog());
            const sendBtn = document.getElementById('sv-send-notification-btn');
            if (sendBtn) sendBtn.addEventListener('click', () => this.sendNotification());
        }

        // "Go to Files" button in empty state
        const goToFilesBtn = document.getElementById('go-to-files-btn');
        if (goToFilesBtn) {
            goToFilesBtn.addEventListener('click', () => {
                if (switchToFilesSection) switchToFilesSection();
            });
        }
    },

    // Initialize a custom select dropdown
    _initCustomSelect(wrapperId, toggleId, dropdownId) {
        const wrapper = document.getElementById(wrapperId);
        const toggle = document.getElementById(toggleId);
        const dropdown = document.getElementById(dropdownId);
        if (!wrapper || !toggle || !dropdown) return;

        toggle.addEventListener('click', (e) => {
            e.stopPropagation();
            // Close other open selects
            document.querySelectorAll('.shared-custom-select.open').forEach((sel) => {
                if (sel !== wrapper) sel.classList.remove('open');
            });
            wrapper.classList.toggle('open');
        });

        dropdown.querySelectorAll('.shared-select-option').forEach((option) => {
            option.addEventListener('click', (e) => {
                e.stopPropagation();
                // Update active state
                dropdown.querySelectorAll('.shared-select-option').forEach((o) => {
                    o.classList.remove('active');
                });
                option.classList.add('active');
                // Update label
                const label = toggle.querySelector('.shared-select-label');
                if (label) label.textContent = option.textContent;
                // Close dropdown
                wrapper.classList.remove('open');
                // Trigger filter
                this.filterAndSortItems();
            });
        });
    },

    // Filter and sort items
    filterAndSortItems() {
        const filterTypeActive = document.querySelector('#filter-type-dropdown .shared-select-option.active');
        const sortByActive = document.querySelector('#sort-by-dropdown .shared-select-option.active');

        const type = filterTypeActive ? filterTypeActive.dataset.value : 'all';
        const sort = sortByActive ? sortByActive.dataset.value : 'date';

        // Use the main top-bar search input
        const searchInput = document.getElementById('search-input');
        const searchTerm = searchInput ? searchInput.value.toLowerCase() : '';

        this.filteredItems = this.items.filter((item) => {
            if (type !== 'all' && item.item_type !== type) return false;
            const name = (item.item_name || item.item_id || '').toLowerCase();
            return name.includes(searchTerm);
        });

        this.filteredItems.sort((a, b) => {
            if (sort === 'name') {
                return (a.item_name || a.item_id || '').localeCompare(b.item_name || b.item_id || '');
            } else if (sort === 'date') {
                return (b.created_at || 0) - (a.created_at || 0);
            } else if (sort === 'expiration') {
                if (!a.expires_at && !b.expires_at) return 0;
                if (!a.expires_at) return 1;
                if (!b.expires_at) return -1;
                return a.expires_at - b.expires_at;
            }
            return 0;
        });

        this.displaySharedItems();
    },

    // Display items in the table
    displaySharedItems() {
        const sharedItemsList = document.getElementById('shared-items-list');
        const emptyState = document.getElementById('empty-shared-state');
        const listContainer = document.querySelector('.shared-list-container');

        if (!sharedItemsList || !emptyState || !listContainer) return;
        sharedItemsList.innerHTML = '';

        if (this.filteredItems.length === 0) {
            emptyState.classList.remove('hidden');
            listContainer.classList.add('hidden');
            return;
        }

        emptyState.classList.add('hidden');
        listContainer.classList.remove('hidden');

        this.filteredItems.forEach((item) => {
            const row = document.createElement('tr');
            const displayName = item.item_name || item.item_id || 'Unknown';

            const nameCell = document.createElement('td');
            nameCell.className = 'shared-item-name';
            const iconSpan = document.createElement('span');
            iconSpan.className = 'item-icon';
            iconSpan.textContent = item.item_type === 'file' ? '📄' : '📁';
            const nameSpan = document.createElement('span');
            nameSpan.textContent = displayName;
            nameCell.appendChild(iconSpan);
            nameCell.appendChild(nameSpan);

            const typeCell = document.createElement('td');
            typeCell.textContent = item.item_type === 'file' ? this.translate('shared_typeFile', 'File') : this.translate('shared_typeFolder', 'Folder');

            const dateCell = document.createElement('td');
            dateCell.textContent = this.formatDate(item.created_at);

            const expCell = document.createElement('td');
            expCell.textContent = item.expires_at ? this.formatDate(item.expires_at) : this.translate('shared_noExpiration', 'No expiration');

            const permCell = document.createElement('td');
            const perms = [];
            if (item.permissions?.read) perms.push(this.translate('share_permissionRead', 'Read'));
            if (item.permissions?.write) perms.push(this.translate('share_permissionWrite', 'Write'));
            if (item.permissions?.reshare) perms.push(this.translate('share_permissionReshare', 'Reshare'));
            permCell.textContent = perms.join(', ') || 'Read';

            const pwCell = document.createElement('td');
            pwCell.textContent = item.has_password ? this.translate('shared_hasPassword', 'Yes') : this.translate('shared_noPassword', 'No');

            const actionsCell = document.createElement('td');
            actionsCell.className = 'shared-item-actions';

            const editBtn = document.createElement('button');
            editBtn.className = 'action-btn edit-btn';
            editBtn.innerHTML = '<span class="action-icon">✏️</span>';
            editBtn.title = this.translate('shared_editShare', 'Edit Share');
            editBtn.addEventListener('click', () => this.openShareDialog(item));

            const notifyBtn = document.createElement('button');
            notifyBtn.className = 'action-btn notify-btn';
            notifyBtn.innerHTML = '<span class="action-icon">📧</span>';
            notifyBtn.title = this.translate('shared_notifyShare', 'Notify Someone');
            notifyBtn.addEventListener('click', () => this.openNotificationDialog(item));

            const copyBtn = document.createElement('button');
            copyBtn.className = 'action-btn copy-btn';
            copyBtn.innerHTML = '<span class="action-icon">📋</span>';
            copyBtn.title = this.translate('shared_copyLink', 'Copy Link');
            copyBtn.addEventListener('click', () => {
                navigator.clipboard
                    .writeText(item.url)
                    .then(() => this.showNotification(this.translate('shared_linkCopied', 'Link copied!')))
                    .catch(() => this.showNotification(this.translate('shared_linkCopyFailed', 'Failed to copy link'), 'error'));
            });

            const rmBtn = document.createElement('button');
            rmBtn.className = 'action-btn remove-btn';
            rmBtn.innerHTML = '<span class="action-icon">🗑️</span>';
            rmBtn.title = this.translate('shared_removeShare', 'Remove Share');
            rmBtn.addEventListener('click', () => {
                this.currentItem = item;
                this.removeSharedItem();
            });

            actionsCell.append(editBtn, notifyBtn, copyBtn, rmBtn);
            row.append(nameCell, typeCell, dateCell, expCell, permCell, pwCell, actionsCell);
            sharedItemsList.appendChild(row);
        });
    },

    // Open share dialog
    openShareDialog(item) {
        this.currentItem = item;
        const shareDialog = document.getElementById('shared-view-edit-dialog');
        const dn = item.item_name || item.item_id || 'Unknown';

        const iconEl = document.getElementById('sv-dialog-icon');
        const nameEl = document.getElementById('sv-dialog-name');
        const urlEl = document.getElementById('sv-share-link-url');
        const enablePw = document.getElementById('sv-enable-password');
        const pwField = document.getElementById('sv-share-password');
        const enableExp = document.getElementById('sv-enable-expiration');
        const expField = document.getElementById('sv-share-expiration');
        const permRead = document.getElementById('sv-permission-read');
        const permWrite = document.getElementById('sv-permission-write');
        const permReshare = document.getElementById('sv-permission-reshare');

        if (!shareDialog) return;
        if (iconEl) iconEl.textContent = item.item_type === 'file' ? '📄' : '📁';
        if (nameEl) nameEl.textContent = dn;
        if (urlEl) urlEl.value = item.url || '';

        if (permRead) permRead.checked = item.permissions?.read !== false;
        if (permWrite) permWrite.checked = !!item.permissions?.write;
        if (permReshare) permReshare.checked = !!item.permissions?.reshare;

        if (enablePw) {
            enablePw.checked = item.has_password;
            if (pwField) {
                pwField.disabled = !enablePw.checked;
                pwField.value = '';
            }
        }
        if (enableExp) {
            enableExp.checked = !!item.expires_at;
            if (expField) {
                expField.disabled = !enableExp.checked;
                expField.value = item.expires_at ? new Date(item.expires_at * 1000).toISOString().split('T')[0] : '';
            }
        }

        shareDialog.classList.remove('hidden');
    },

    closeShareDialog() {
        const d = document.getElementById('shared-view-edit-dialog');
        if (d) d.classList.add('hidden');
        this.currentItem = null;
    },

    openNotificationDialog(item) {
        this.currentItem = item;
        const dn = item.item_name || item.item_id || 'Unknown';
        const d = document.getElementById('sv-notification-dialog');
        const iconEl = document.getElementById('sv-notify-dialog-icon');
        const nameEl = document.getElementById('sv-notify-dialog-name');
        const emailEl = document.getElementById('sv-notification-email');
        const msgEl = document.getElementById('sv-notification-message');

        if (!d) return;
        if (iconEl) iconEl.textContent = item.item_type === 'file' ? '📄' : '📁';
        if (nameEl) nameEl.textContent = dn;
        if (emailEl) emailEl.value = '';
        if (msgEl) msgEl.value = '';
        d.classList.remove('hidden');
    },

    closeNotificationDialog() {
        const d = document.getElementById('sv-notification-dialog');
        if (d) d.classList.add('hidden');
        this.currentItem = null;
    },

    copyShareLink() {
        const el = document.getElementById('sv-share-link-url');
        if (!el) return;
        navigator.clipboard
            .writeText(el.value)
            .then(() => this.showNotification(this.translate('shared_linkCopied', 'Link copied!')))
            .catch(() => this.showNotification(this.translate('shared_linkCopyFailed', 'Failed to copy link'), 'error'));
    },

    // Generate secure password with crypto API
    generatePassword() {
        const pwField = document.getElementById('sv-share-password');
        const enablePw = document.getElementById('sv-enable-password');
        if (!pwField || !enablePw) return;

        const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*';
        const array = new Uint32Array(16);
        crypto.getRandomValues(array);
        let password = '';
        for (let i = 0; i < 16; i++) {
            password += chars[array[i] % chars.length];
        }
        pwField.value = password;
        enablePw.checked = true;
        pwField.disabled = false;
    },

    // Update share via API
    async updateSharedItem() {
        if (!this.currentItem) return;

        const permRead = document.getElementById('sv-permission-read');
        const permWrite = document.getElementById('sv-permission-write');
        const permReshare = document.getElementById('sv-permission-reshare');
        const enablePw = document.getElementById('sv-enable-password');
        const pwField = document.getElementById('sv-share-password');
        const enableExp = document.getElementById('sv-enable-expiration');
        const expField = document.getElementById('sv-share-expiration');

        const body = {
            permissions: {
                read: permRead ? permRead.checked : true,
                write: permWrite ? permWrite.checked : false,
                reshare: permReshare ? permReshare.checked : false
            },
            password: enablePw?.checked && pwField?.value ? pwField.value : null,
            expires_at: enableExp?.checked && expField?.value ? Math.floor(new Date(expField.value).getTime() / 1000) : null
        };

        try {
            // FIXME: redundance with fileSharing
            const res = await fetch(`/api/shares/${this.currentItem.id}`, {
                method: 'PUT',
                headers: this._headers(true),
                body: JSON.stringify(body)
            });
            if (!res.ok) {
                const err = await res.json().catch(() => ({}));
                throw new Error(err.error || `Server error ${res.status}`);
            }
            this.showNotification(this.translate('shared_itemUpdated', 'Share settings updated'));
        } catch (err) {
            console.error('Error updating share:', err);
            this.showNotification(err.message || 'Error updating share', 'error');
        }
        // update UI
        ui.setSharedVisualState(this.currentItem.item_id, this.currentItem.item_type, true);
        this.closeShareDialog();
        await this.loadItems(true);
        this.filterAndSortItems();
    },

    // Remove share via API
    async removeSharedItem() {
        if (!this.currentItem) return;

        try {
            // FIXME: redundance with fileSharing
            const res = await fetch(`/api/shares/${this.currentItem.id}`, {
                method: 'DELETE',
                headers: this._headers()
            });
            if (!res.ok && res.status !== 204) throw new Error(`Server error ${res.status}`);
            this.showNotification(this.translate('shared_itemRemoved', 'Share removed'));
        } catch (err) {
            console.error('Error removing share:', err);
            this.showNotification('Error removing share', 'error');
        }

        this.closeShareDialog();
        await this.loadItems(true);
        this.filterAndSortItems();
        // update UI
        ui.setSharedVisualState(this.currentItem.item_id, this.currentItem.item_type, this.isShared(this.currentItem.item_id, this.currentItem.item_type));
    },

    // Send notification (stub)
    sendNotification() {
        if (!this.currentItem) return;
        const emailEl = document.getElementById('sv-notification-email');
        const msgEl = document.getElementById('sv-notification-message');
        const email = emailEl ? emailEl.value.trim() : '';
        const message = msgEl ? msgEl.value.trim() : '';

        if (!email || !this.validateEmail(email)) {
            this.showNotification(this.translate('shared_invalidEmail', 'Please enter a valid email address'), 'error');
            return;
        }

        if (fileSharing?.sendShareNotification) {
            fileSharing
                .sendShareNotification(this.currentItem.url, email, message)
                .then(() => {
                    this.closeNotificationDialog();
                    this.showNotification(this.translate('shared_notificationSent', 'Notification sent'));
                })
                .catch(() => this.showNotification(this.translate('shared_notificationFailed', 'Failed to send notification'), 'error'));
        }
    },

    showNotification(message, type = 'success') {
        if (ui?.showNotification) {
            ui.showNotification(message, type);
        } else {
            alert(message);
        }
    },

    validateEmail(email) {
        return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email);
    },

    formatDate(value) {
        return formatDateShort(value);
    },

    translate(key, defaultText) {
        return i18n.t(key, defaultText);
    }
};

export { sharedView };
