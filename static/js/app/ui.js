/**
 * OxiCloud - UI Module
 * This file handles UI-related functions, view toggling, and interface interactions
 */

// @ts-check

import { escapeHtml, formatDateTime, formatFileSize } from '../core/formatters.js';
import { i18n } from '../core/i18n.js';
import { OxiIcons } from '../core/icons.js';
import { contextMenus } from '../features/files/contextMenus.js';
import { fileOps } from '../features/files/fileOperations.js';
import { inlineViewer } from '../features/files/inlineViewer.js';
import { multiSelect } from '../features/files/multiSelect.js';
import { wopiEditor } from '../features/files/wopiEditor.js';
import { favorites } from '../features/library/favorites.js';
import { recent } from '../features/library/recent.js';
import { fileSharing } from '../features/sharing/fileSharing.js';
import { sharedView } from '../views/shared/sharedView.js';
import { loadFiles } from './filesView.js';
import { updateHistory } from './main.js';
import { syncViewContainers } from './navigation.js';
import { app } from './state.js';
import { uiFileTypes } from './uiFileTypes.js';
import { uiNotifications } from './uiNotifications.js';

let __rubberBandJustFinished = false;

// UI Module
const ui = {
    /** @type {HTMLDListElement | null} */
    //dragPreview,

    /**
     * Initialize context menus and dialogs
     */
    initializeContextMenus() {
        // Folder context menu
        if (!document.getElementById('folder-context-menu')) {
            const folderMenu = document.createElement('div');
            folderMenu.classList.add('context-menu', 'hidden');
            folderMenu.id = 'folder-context-menu';
            folderMenu.innerHTML = `
                <div class="context-menu-item" id="download-folder-option">
                    <i class="fas fa-download"></i> <span data-i18n="actions.download">Download</span>
                </div>
                <div class="context-menu-item" id="favorite-folder-option">
                    <i class="fas fa-star"></i> <span data-i18n="actions.favorite">Add to favorites</span>
                </div>
                <div class="context-menu-item" id="share-folder-option">
                    <i class="fas fa-share-alt"></i> <span data-i18n="actions.share">Share</span>
                </div>
                <div class="context-menu-separator"></div>
                <div class="context-menu-item" id="rename-folder-option">
                    <i class="fas fa-pen"></i> <span data-i18n="actions.rename">Rename</span>
                </div>
                <div class="context-menu-item" id="move-folder-option">
                    <i class="fas fa-arrows-alt"></i> <span data-i18n="actions.move">Move to...</span>
                </div>
                <div class="context-menu-separator"></div>
                <div class="context-menu-item context-menu-item-danger" id="delete-folder-option">
                    <i class="fas fa-trash-alt"></i> <span data-i18n="actions.delete">Delete</span>
                </div>
            `;
            document.body.appendChild(folderMenu);
        }

        // File context menu
        if (!document.getElementById('file-context-menu')) {
            const fileMenu = document.createElement('div');
            fileMenu.classList.add('context-menu', 'hidden');
            fileMenu.id = 'file-context-menu';
            fileMenu.innerHTML = `
                <div class="context-menu-item" id="view-file-option">
                    <i class="fas fa-eye"></i> <span data-i18n="actions.view">View</span>
                </div>
                <div class="context-menu-item hidden" id="wopi-edit-file-option">
                    <i class="fas fa-file-word"></i> <span>Edit in Office</span>
                </div>
                <div class="context-menu-item hidden" id="wopi-edit-file-tab-option">
                    <i class="fas fa-external-link-alt"></i> <span>Edit in Office (new tab)</span>
                </div>
                <div class="context-menu-item" id="download-file-option">
                    <i class="fas fa-download"></i> <span data-i18n="actions.download">Download</span>
                </div>
                <div class="context-menu-separator"></div>
                <div class="context-menu-item" id="favorite-file-option">
                    <i class="fas fa-star"></i> <span data-i18n="actions.favorite">Add to favorites</span>
                </div>
                <div class="context-menu-item" id="share-file-option">
                    <i class="fas fa-share-alt"></i> <span data-i18n="actions.share">Share</span>
                </div>
                <div class="context-menu-item hidden" id="add-to-playlist-option">
                    <i class="fas fa-compact-disc"></i> <span data-i18n="music.add_to_playlist">Add to Playlist</span>
                </div>
                <div class="context-menu-separator"></div>
                <div class="context-menu-item" id="rename-file-option">
                    <i class="fas fa-pen"></i> <span data-i18n="actions.rename">Rename</span>
                </div>
                <div class="context-menu-item" id="move-file-option">
                    <i class="fas fa-arrows-alt"></i> <span data-i18n="actions.move">Move to...</span>
                </div>
                <div class="context-menu-separator"></div>
                <div class="context-menu-item context-menu-item-danger" id="delete-file-option">
                    <i class="fas fa-trash-alt"></i> <span data-i18n="actions.delete">Delete</span>
                </div>
            `;
            document.body.appendChild(fileMenu);
        }

        // Rename dialog — modern
        if (!document.getElementById('rename-dialog')) {
            const renameDialog = document.createElement('div');
            renameDialog.classList.add('rename-dialog', 'hidden');
            renameDialog.id = 'rename-dialog';
            renameDialog.innerHTML = `
                <div class="rename-dialog-content">
                    <div class="rename-dialog-header">
                        <i class="fas fa-pen dialog-header-icon"></i>
                        <span data-i18n="dialogs.rename_folder">Rename</span>
                    </div>
                    <div class="rename-dialog-body">
                        <input type="text" id="rename-input" data-i18n-placeholder="dialogs.new_name" placeholder="New name">
                    </div>
                    <div class="rename-dialog-buttons">
                        <button class="btn btn-secondary" id="rename-cancel-btn" data-i18n="actions.cancel">Cancel</button>
                        <button class="btn btn-primary" id="rename-confirm-btn" data-i18n="actions.rename">Rename</button>
                    </div>
                </div>
            `;
            document.body.appendChild(renameDialog);
        }

        // Move dialog — modern with navigation
        if (!document.getElementById('move-file-dialog')) {
            const moveDialog = document.createElement('div');
            moveDialog.classList.add('rename-dialog', 'hidden');
            moveDialog.id = 'move-file-dialog';
            moveDialog.innerHTML = `
                <div class="rename-dialog-content">
                    <div class="rename-dialog-header">
                        <i class="fas fa-arrows-alt dialog-header-icon"></i>
                        <span data-i18n="dialogs.move_file">Move</span>
                    </div>
                    <div class="rename-dialog-body">
                        <p class="move-dialog-hint" data-i18n="dialogs.select_destination">Select destination folder:</p>
                        <div id="move-dialog-breadcrumb" class="move-dialog-breadcrumb"></div>
                        <div id="folder-select-container" class="folder-select-container">
                        </div>
                    </div>
                    <div class="rename-dialog-buttons">
                        <button class="btn btn-secondary" id="move-cancel-btn" data-i18n="actions.cancel">Cancel</button>
                        <button class="btn btn-outline" id="copy-confirm-btn" data-i18n="actions.copy">Copy</button>
                        <button class="btn btn-primary" id="move-confirm-btn" data-i18n="actions.move_to">Move</button>
                    </div>
                </div>
            `;
            document.body.appendChild(moveDialog);
        }

        // Share dialog
        if (!document.getElementById('share-dialog')) {
            const shareDialog = document.createElement('div');
            shareDialog.classList.add('share-dialog', 'hidden');
            shareDialog.id = 'share-dialog';
            shareDialog.innerHTML = `
                <div class="share-dialog-content">
                    <div class="share-dialog-header">
                        <i class="fas fa-share-alt dialog-header-icon"></i>
                        <span data-i18n="dialogs.share_file">Share file</span>
                    </div>
                    <div class="shared-item-info">
                        <strong>Item:</strong> <span id="shared-item-name"></span>
                    </div>

                    <div id="existing-shares-section" class="share-section hidden">
                        <h3 data-i18n="dialogs.existing_shares">Existing shared links</h3>
                        <div id="existing-shares-container"></div>
                    </div>

                    <div class="share-options">
                        <h3 data-i18n="dialogs.share_options">Share options</h3>

                        <div class="form-group">
                            <label for="share-password" data-i18n="dialogs.password">Password (optional):</label>
                            <input type="password" id="share-password" placeholder="Protect with password">
                        </div>

                        <div class="form-group">
                            <label for="share-expiration" data-i18n="dialogs.expiration">Expiration date (optional):</label>
                            <input type="date" id="share-expiration">
                        </div>

                        <div class="form-group">
                            <label data-i18n="dialogs.permissions">Permissions:</label>
                            <div class="permission-options">
                                <div class="permission-option">
                                    <input type="checkbox" id="share-permission-read" checked>
                                    <label for="share-permission-read" data-i18n="permissions.read">Read</label>
                                </div>
                                <div class="permission-option">
                                    <input type="checkbox" id="share-permission-write">
                                    <label for="share-permission-write" data-i18n="permissions.write">Write</label>
                                </div>
                                <div class="permission-option">
                                    <input type="checkbox" id="share-permission-reshare">
                                    <label for="share-permission-reshare" data-i18n="permissions.reshare">Allow sharing</label>
                                </div>
                            </div>
                        </div>
                        <button class="btn btn-primary btn-small" id="share-confirm-btn" data-i18n="actions.share">Share</button>
                    </div>

                    <div id="new-share-section" class="share-section hidden">
                        <h3 data-i18n="dialogs.generated_link">Generated link</h3>
                        <div class="form-group">
                            <input type="text" id="generated-share-url" readonly>
                            <div class="share-link-actions">
                                <button class="btn btn-small" id="copy-share-btn">
                                    <i class="fas fa-copy"></i> <span data-i18n="actions.copy">Copy</span>
                                </button>
                                <button class="btn btn-small" id="notify-share-btn">
                                    <i class="fas fa-envelope"></i> <span data-i18n="actions.notify">Notify</span>
                                </button>
                            </div>
                        </div>
                    </div>

                    <div class="share-dialog-buttons">
                        <button class="btn btn-secondary" id="share-close-btn" data-i18n="actions.close">Close</button>
                    </div>
                </div>
            `;
            i18n.translateElement(shareDialog);
            document.body.appendChild(shareDialog);

            // Add event listeners for share dialog
            document.getElementById('share-close-btn').addEventListener('click', () => {
                contextMenus.closeShareDialog();
            });

            document.getElementById('share-confirm-btn').addEventListener('click', async () => {
                await contextMenus.createSharedLink();
            });

            document.getElementById('copy-share-btn').addEventListener('click', async () => {
                const shareUrl = document.getElementById('generated-share-url').value;
                await fileSharing.copyLinkToClipboard(shareUrl);
            });

            document.getElementById('notify-share-btn').addEventListener('click', () => {
                const shareUrl = document.getElementById('generated-share-url').value;
                contextMenus.showEmailNotificationDialog(shareUrl);
            });

            // FIXME make generic function (close all dialog / etc)
            document.addEventListener('keydown', (e) => {
                const dialog = document.getElementById('share-dialog');
                if (e.key === 'Escape' && !dialog?.classList.contains('hidden')) {
                    contextMenus.closeShareDialog();
                }
            });

            shareDialog.addEventListener('click', (e) => {
                if (e.target === shareDialog) {
                    contextMenus.closeShareDialog();
                }
            });
        }

        // Notification dialog
        if (!document.getElementById('notification-dialog')) {
            const notificationDialog = document.createElement('div');
            notificationDialog.classList.add('share-dialog', 'hidden');
            notificationDialog.id = 'notification-dialog';
            notificationDialog.innerHTML = `
                <div class="share-dialog-content">
                    <div class="share-dialog-header">
                        <i class="fas fa-envelope dialog-header-icon"></i>
                        <span data-i18n="dialogs.notify">Notify shared link</span>
                    </div>

                    <p><strong>URL:</strong> <span id="notification-share-url"></span></p>

                    <div class="form-group">
                        <label for="notification-email" data-i18n="dialogs.recipient">Recipient:</label>
                        <input type="email" id="notification-email" placeholder="Email address">
                    </div>

                    <div class="form-group">
                        <label for="notification-message" data-i18n="dialogs.message">Message (optional):</label>
                        <textarea id="notification-message" rows="3"></textarea>
                    </div>

                    <div class="share-dialog-buttons">
                        <button class="btn btn-secondary" id="notification-cancel-btn" data-i18n="actions.cancel">Cancel</button>
                        <button class="btn btn-primary" id="notification-send-btn" data-i18n="actions.send">Send</button>
                    </div>
                </div>
            `;
            document.body.appendChild(notificationDialog);

            // Add event listeners for notification dialog
            document.getElementById('notification-cancel-btn').addEventListener('click', () => {
                contextMenus.closeNotificationDialog();
            });

            document.getElementById('notification-send-btn').addEventListener('click', () => {
                contextMenus.sendShareNotification();
            });
        }

        // Playlist selection dialog
        if (!document.getElementById('playlist-dialog')) {
            const playlistDialog = document.createElement('div');
            playlistDialog.classList.add('share-dialog', 'hidden');
            playlistDialog.id = 'playlist-dialog';
            playlistDialog.innerHTML = `
                <div class="share-dialog-content">
                    <div class="share-dialog-header">
                        <i class="fas fa-music dialog-header-icon"></i>
                        <span data-i18n="music.add_to_playlist">Add to Playlist</span>
                    </div>
                    <div id="playlist-dialog-files-info" class="shared-item-info"></div>
                    <div id="playlist-select-container" class="folder-select-container">
                    </div>
                    <div class="rename-dialog-buttons">
                        <button class="btn btn-secondary" id="playlist-cancel-btn" data-i18n="actions.cancel">Cancel</button>
                        <button class="btn btn-primary" id="playlist-add-btn" data-i18n="music.add">Add</button>
                    </div>
                </div>
            `;
            document.body.appendChild(playlistDialog);

            document.getElementById('playlist-cancel-btn').addEventListener('click', () => {
                if (contextMenus) contextMenus.closePlaylistDialog();
            });
        }

        // Assign events to menu items
        if (contextMenus) {
            contextMenus.assignMenuEvents();
        } else {
            console.warn('contextMenus module not loaded');
        }
    },

    /**
     * Set up drag and drop functionality
     */
    setupDragAndDrop() {
        // prepare area to build dragged elements
        this.dragPreview = document.createElement('div');
        this.dragPreview.className = 'drag-preview';
        document.body.appendChild(this.dragPreview);
        this.draggedItems = null;

        const dropzone = document.getElementById('dropzone');

        const collectDroppedEntries = async (dataTransfer) => {
            const items = Array.from(dataTransfer?.items || []);
            const rootEntries = items.map((it) => (typeof it.webkitGetAsEntry === 'function' ? it.webkitGetAsEntry() : null)).filter(Boolean);

            if (rootEntries.length === 0) return null;

            const out = [];

            const walkEntry = async (entry, prefix = '') => {
                if (!entry) return;

                if (entry.isFile) {
                    await new Promise((resolve) => {
                        entry.file(
                            (file) => {
                                out.push({ file, relativePath: `${prefix}${file.name}` });
                                resolve();
                            },
                            () => resolve()
                        );
                    });
                    return;
                }

                if (entry.isDirectory) {
                    const dirPrefix = `${prefix}${entry.name}/`;
                    const reader = entry.createReader();

                    while (true) {
                        const children = await new Promise((resolve) => {
                            reader.readEntries(resolve, () => resolve([]));
                        });
                        if (!children || children.length === 0) break;
                        for (const child of children) {
                            // eslint-disable-next-line no-await-in-loop
                            await walkEntry(child, dirPrefix);
                        }
                    }
                }
            };

            for (const root of rootEntries) {
                // eslint-disable-next-line no-await-in-loop
                await walkEntry(root, '');
            }

            return out;
        };

        // Dropzone events
        dropzone.addEventListener('dragover', (e) => {
            e.preventDefault();
            dropzone.classList.add('active');
        });

        dropzone.addEventListener('dragleave', () => {
            dropzone.classList.remove('active');
        });

        dropzone.addEventListener('drop', async (e) => {
            e.preventDefault();
            e.stopPropagation(); // Prevent bubbling to document's drop handler (avoids double upload)
            e._oxiHandled = true; // Mark as handled for document-level fallback
            dropzone.classList.remove('active');
            if (e.dataTransfer.files.length > 0) {
                // First try directory-aware extraction (Finder folder drag & drop)
                const droppedEntries = await collectDroppedEntries(e.dataTransfer);
                if (droppedEntries && droppedEntries.length > 0) {
                    const hasFolderStructure = droppedEntries.some((x) => x.relativePath?.includes('/'));
                    if (hasFolderStructure) {
                        fileOps.uploadFolderEntries(droppedEntries);
                    } else {
                        fileOps.uploadFiles(droppedEntries.map((x) => x.file));
                    }
                    setTimeout(() => {
                        dropzone?.classList.add('hidden');
                    }, 500);
                    return;
                }

                // Detect folder drops: files from folder drops have webkitRelativePath set
                const hasRelativePaths = Array.from(e.dataTransfer.files).some((f) => f.webkitRelativePath?.includes('/'));
                if (hasRelativePaths) {
                    fileOps.uploadFolderFiles(e.dataTransfer.files);
                } else {
                    fileOps.uploadFiles(e.dataTransfer.files);
                }
            }
            setTimeout(() => {
                dropzone?.classList.add('hidden');
            }, 500);
        });

        // Document-wide drag and drop
        document.addEventListener('dragover', (e) => {
            e.preventDefault();
            if (e.dataTransfer.types.includes('Files')) {
                dropzone?.classList.remove('hidden');
                dropzone?.classList.add('active');
            }
        });

        document.addEventListener('dragleave', (e) => {
            if (e.clientX <= 0 || e.clientY <= 0 || e.clientX >= window.innerWidth || e.clientY >= window.innerHeight) {
                dropzone.classList.remove('active');
                setTimeout(() => {
                    if (!dropzone.classList.contains('active')) {
                        dropzone?.classList.add('hidden');
                    }
                }, 100);
            }
        });

        document.addEventListener('drop', async (e) => {
            e.preventDefault();
            dropzone.classList.remove('active');

            // Skip if already handled by the dropzone handler (defensive against bubble leaks)
            if (e._oxiHandled) return;

            if (e.dataTransfer.files.length > 0) {
                // First try directory-aware extraction (Finder folder drag & drop)
                const droppedEntries = await collectDroppedEntries(e.dataTransfer);
                if (droppedEntries && droppedEntries.length > 0) {
                    const hasFolderStructure = droppedEntries.some((x) => x.relativePath?.includes('/'));
                    if (hasFolderStructure) {
                        fileOps.uploadFolderEntries(droppedEntries);
                    } else {
                        fileOps.uploadFiles(droppedEntries.map((x) => x.file));
                    }
                    setTimeout(() => {
                        dropzone?.classList.add('hidden');
                    }, 500);
                    return;
                }

                // Detect folder drops: files from folder drops have webkitRelativePath set
                const hasRelativePaths = Array.from(e.dataTransfer.files).some((f) => f.webkitRelativePath?.includes('/'));
                if (hasRelativePaths) {
                    fileOps.uploadFolderFiles(e.dataTransfer.files);
                } else {
                    fileOps.uploadFiles(e.dataTransfer.files);
                }
            }

            setTimeout(() => {
                dropzone?.classList.add('hidden');
            }, 500);
        });
    },

    /**
     * Switch to grid view
     */
    switchToGridView() {
        this._hydrateViewIfNeeded();

        app.currentView = 'grid';
        localStorage.setItem('oxicloud-view', 'grid');

        syncViewContainers();
    },

    /**
     * Switch to list view
     */
    switchToListView() {
        this._hydrateViewIfNeeded();

        app.currentView = 'list';
        localStorage.setItem('oxicloud-view', 'list');

        syncViewContainers();
    },

    /**
     * Update breadcrumb navigation from the breadcrumbPath array.
     * Renders: Home > folder1 > folder2 > ...
     * Each segment is clickable to navigate back to that level.
     */
    updateBreadcrumb() {
        const breadcrumb = document.querySelector('.breadcrumb');
        breadcrumb.innerHTML = '';
        const path = app.breadcrumbPath; // [{id, name}, ...]

        // -- Home icon (always present, clickable to go to root) --
        const homeIcon = document.createElement('span');
        homeIcon.className = 'breadcrumb-item breadcrumb-home';
        homeIcon.innerHTML = '<i class="fas fa-home"></i>';
        homeIcon.title = i18n.t('breadcrumb.home');

        // Home is always clickable if we have a home folder
        if (app.userHomeFolderId) {
            homeIcon.classList.add('breadcrumb-link');
            homeIcon.addEventListener('click', () => {
                app.breadcrumbPath = [];
                app.currentPath = app.userHomeFolderId;
                this.updateBreadcrumb();
                loadFiles();
            });
        }
        breadcrumb.appendChild(homeIcon);

        // -- Root/Home folder name (if available) is always the first element of the breadcrumb --
        // TODO clarify the difference between homeIcon & this first element
        if (app.userHomeFolderName) {
            if (path.length === 0 || path[0].id !== app.userHomeFolderId) {
                path.unshift({
                    name: app.userHomeFolderName,
                    id: app.userHomeFolderId
                });
            }
        }

        // -- Root/Home + Intermediate + current segments --
        path.forEach((segment, index) => {
            const isLast = index === path.length - 1;

            // Separator
            const separator = document.createElement('span');
            separator.className = 'breadcrumb-separator';
            separator.textContent = '>';
            breadcrumb.appendChild(separator);

            // Segment item
            const item = document.createElement('span');
            item.className = 'breadcrumb-item';
            item.textContent = segment.name;
            item.dataset.folderId = segment.id;

            if (!isLast) {
                // Intermediate segment: clickable – truncate path to this level
                item.classList.add('breadcrumb-link');
                item.addEventListener('click', () => {
                    app.breadcrumbPath = path.slice(0, index + 1);
                    app.currentPath = segment.id;
                    this.updateBreadcrumb();
                    loadFiles();
                });

                // can drag files on this folder
                // dragover – only folders are valid drop targets
                item.addEventListener('dragover', (e) => {
                    const card = e.target.closest('span');
                    if (!card?.dataset.folderId) return;
                    e.preventDefault();
                    card.classList.add('drop-target');
                });

                // dragleave
                item.addEventListener('dragleave', (e) => {
                    console.log('dragleave ', e);
                    const card = e.target.closest('span');
                    if (!card?.dataset.folderId) return;
                    card.classList.remove('drop-target');
                });

                // drop – only folders accept drops
                item.addEventListener('drop', async (e) => {
                    const card = e.target.closest('span');
                    if (!card) return;
                    const targetFolderId = card.dataset.folderId;
                    if (!targetFolderId) return;

                    e.preventDefault();
                    card.classList.remove('drop-target');

                    const action = e.dataTransfer?.dropEffect;
                    await this._dropToFolder(action, targetFolderId, e.dataTransfer);
                });
            } else {
                // Last segment: current location, not clickable
                item.classList.add('breadcrumb-current');
            }
            breadcrumb.appendChild(item);
        });
    },

    /**
     * Check if a file can be previewed in the viewer
     * @param {Object} file - File object with mime_type property
     * @returns {boolean}
     */
    isViewableFile(file) {
        return uiFileTypes.isViewableFile(file);
    },

    /**
     * Get FontAwesome icon class for a filename based on its extension.
     * Used as fallback when the backend DTO doesn't include icon_class
     * (e.g. trash items).
     */
    getIconClass(fileName) {
        return uiFileTypes.getIconClass(fileName);
    },

    /**
     * Get CSS special class for icon styling based on filename extension.
     * Used as fallback when the backend DTO doesn't include icon_special_class.
     */
    getIconSpecialClass(fileName) {
        return uiFileTypes.getIconSpecialClass(fileName);
    },

    /**
     * Show notification
     * @param {string} title - Notification title
     * @param {string} message - Notification message
     */
    showNotification(title, message) {
        uiNotifications.show(title, message);
    },

    /**
     * Close folder context menu
     */
    closeContextMenu() {
        const menu = document.getElementById('folder-context-menu');
        if (menu) {
            menu.classList.add('hidden');
            app.contextMenuTargetFolder = null;
        }
    },

    /**
     * Close file context menu
     */
    closeFileContextMenu() {
        const menu = document.getElementById('file-context-menu');
        if (menu) {
            menu.classList.add('hidden');
            app.contextMenuTargetFile = null;
        }
    },

    /* ================================================================
     *  Data store + event delegation (replaces per-item listeners)
     * ================================================================ */

    /** @type {Map<string, Object>} item data keyed by id */
    _items: new Map(),

    /** @type {Array<Object>} last rendered folder dataset */
    _lastFolders: [],

    /** @type {Array<Object>} last rendered file dataset */
    _lastFiles: [],

    /** @type {boolean} */
    _delegationReady: false,

    _getActiveView() {
        if (app && app.currentView === 'list') return 'list';
        if (app && app.currentView === 'grid') return 'grid';

        const stored = localStorage.getItem('oxicloud-view');
        return stored === 'list' ? 'list' : 'grid';
    },

    /**
     *
     * @param {Object[]} folders
     * @returns
     */
    _renderFoldersToView(folders) {
        if (!Array.isArray(folders) || folders.length === 0) return;
        const target = document.getElementById('files-list');
        if (!target) return;

        const frag = document.createDocumentFragment();
        for (const folder of folders) {
            frag.appendChild(this._createFolderItem(folder));
        }
        target.appendChild(frag);
    },

    /**
     *
     * @param {Object[]} files
     * @returns
     */
    _renderFilesToView(files) {
        if (!Array.isArray(files) || files.length === 0) return;
        const target = document.getElementById('files-list');
        if (!target) return;

        const frag = document.createDocumentFragment();
        for (const file of files) {
            frag.appendChild(this._createFileItem(file));
        }
        target.appendChild(frag);
    },

    _upsertById(arr, item) {
        if (!Array.isArray(arr) || !item?.id) return;
        const idx = arr.findIndex((x) => x && x.id === item.id);
        if (idx >= 0) {
            arr[idx] = item;
        } else {
            arr.push(item);
        }
    },

    /**
     * handle the drop
     * @param {string} action copy|move
     * @param {string} targetFolderId the target
     * @param {any} dataTransfer fallback if nothing is selected
     */
    async _dropToFolder(action, targetFolderId, dataTransfer) {
        const selection = multiSelect.getSelection(targetFolderId);

        multiSelect.clear();

        if (selection.fileIds.length === 0 && selection.folderIds.length === 0) {
            // try to use dataTransfer (direct move without selection)
            const id = dataTransfer.getData('text/plain');
            const isFolder = dataTransfer.getData('application/oxicloud-folder') === 'true';

            if (isFolder && id === targetFolderId) {
                console.log('nothing to do');
                return; //nothing to do
            }
            // append current item to selection
            if (isFolder) {
                console.log(`adding ${id} as folder`);
                selection.folderIds.push(id);
            } else {
                console.log(`adding ${id} as file`);
                selection.fileIds.push(id);
            }
        }

        console.log(`request ${action} of: `, selection);
        /*
        TODO do we prefer use atomic operation on 1 item ? like:
            await fileOps.moveFolder(sourceId, targetFolderId);
            await fileOps.moveFile(sourceId, targetFolderId);
        */

        let result;
        switch (action) {
            case 'copy':
                result = await fileOps.batchCopy(selection.fileIds, selection.folderIds, targetFolderId);
                break;

            case 'move':
                result = await fileOps.batchMove(selection.fileIds, selection.folderIds, targetFolderId);
                // redraw directory
                if (result.success > 0) loadFiles();
                break;

            default:
                console.error(`drag and drop: action ${action} unknown`);
                return;
        }
        multiSelect.showBatchResult(action, result);
        console.log(result);
    },

    _hydrateViewIfNeeded() {
        // Only hydrate if there is at least one rendered item in the opposite/current DOM.
        // This prevents stale cache hydration in empty-state screens.
        const hasAnyRenderedItem = !!document.querySelector('#files-list .file-item');
        if (!hasAnyRenderedItem) return;

        // FIXME: thre is the header...
        const listView = document.getElementById('files-list');
        if (!listView) return;
        if (listView.children.length > 1) return;

        this._renderFoldersToView(this._lastFolders);
        this._renderFilesToView(this._lastFiles);
    },

    /**
     * Attach a fixed set of delegated event listeners to the two
     * container elements (files-list).
     * Called once – idempotent.
     */
    initDelegation() {
        if (this._delegationReady) return;
        const filesList = document.getElementById('files-list');
        if (!filesList) return;
        this._delegationReady = true;

        // ── helpers ────────────────────────────────────────────────
        const itemInfo = (card) => {
            if (!card) return null;
            const fileId = card.dataset.fileId;
            if (fileId)
                return {
                    type: 'file',
                    id: fileId,
                    name: card.dataset.fileName,
                    data: this._items.get(fileId)
                };
            const folderId = card.dataset.folderId;
            if (folderId)
                return {
                    type: 'folder',
                    id: folderId,
                    name: card.dataset.folderName,
                    data: this._items.get(folderId)
                };
            return null;
        };

        const openFile = async (file) => {
            if (!file) return;
            if (recent) {
                document.dispatchEvent(new CustomEvent('file-accessed', { detail: { file } }));
            }
            // WOPI editor intercept: open Office documents in the WOPI editor
            // But NOT image files - those should be previewed in the inline viewer
            const ext = (file.name || '').split('.').pop().toLowerCase();
            const imageExts = ['jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'bmp', 'ico', 'heic', 'heif', 'avif', 'tiff'];
            const isImage = file.mime_type?.startsWith('image/') || imageExts.includes(ext);
            try {
                if (!isImage && wopiEditor && (await wopiEditor.canEdit(file.name))) {
                    await wopiEditor.openInModal(file.id, file.name, 'edit');
                    return;
                }
            } catch (e) {
                console.warn(`WOPI Editor failed, falling bck to classic view `, e);
            }

            if (this.isViewableFile(file) || isImage) {
                if (inlineViewer) {
                    inlineViewer.openFile(file);
                    // update history
                    app.viewFile = file.id;
                    updateHistory(false);
                } else {
                    fileOps.downloadFile(file.id, file.name);
                }
            } else {
                fileOps.downloadFile(file.id, file.name);
            }
        };

        const navigateFolder = (card) => {
            const folderId = card.dataset.folderId;
            const folderName = card.dataset.folderName;
            app.breadcrumbPath.push({ id: folderId, name: folderName });
            app.currentPath = folderId;
            this.updateBreadcrumb();
            loadFiles();
        };

        const setContextTarget = (card, info) => {
            if (info.type === 'folder') {
                app.contextMenuTargetFolder = {
                    id: info.id,
                    name: card.dataset.folderName,
                    parent_id: card.dataset.parentId || ''
                };
            } else {
                const fileData = info.data || self._items.get(info.id);
                app.contextMenuTargetFile = {
                    id: info.id,
                    name: card.dataset.fileName,
                    folder_id: card.dataset.folderId || '',
                    mime_type: fileData?.mime_type || null
                };
            }
        };

        // ──  click (open / navigate; select only via checkbox) ──
        filesList.addEventListener('click', (e) => {
            const card = e.target.closest('.file-item');
            if (!card) return;

            if (e.target.closest('.file-actions')) {
                e.stopPropagation();
                e.preventDefault();
                const info = itemInfo(card);
                if (!info) return;
                setContextTarget(card, info);
                const menuId = info.type === 'folder' ? 'folder-context-menu' : 'file-context-menu';
                showContextMenuAtElement(e.target.closest('.file-actions'), menuId);
                return;
            }

            if (e.target.closest('.checkbox-cell')) {
                toggleCardSelection(card, e);
                return;
            }

            // Favorite star – handled by direct onclick on the button
            if (e.target.closest('.favorite-star')) return;

            // Single-click opens/navigates (selection is only via checkbox)
            const info = itemInfo(card);
            if (!info) return;

            // use modifier key to select/deselect item
            // note: shift key is used in multiselect
            // note: on MacOS, ctrl Key is used to convert click into right click, which invoke the `contextmenu` event
            if (e.metaKey || e.altKey || e.ctrlKey) {
                toggleCardSelection(card, e);
                return;
            }

            // shiftkey is used to complete selection
            if (e.shiftKey && multiSelect) {
                multiSelect.handleToggleItem(card, e);
                return;
            }

            if (info.type === 'folder') {
                navigateFolder(card);
            } else {
                openFile(info.data);
            }
        });

        // ── GRID: dblclick (navigate / open) ──────────────────────
        filesList.addEventListener('dblclick', (e) => {
            // Single-click already handles open/navigate.
            // Prevent duplicate actions on double-click.
            e.preventDefault();
        });

        // ── shared events ──────────────────────

        filesList.addEventListener('contextmenu', (e) => {
            const card = e.target.closest('.file-item');
            if (!card) return;
            e.preventDefault();
            const info = itemInfo(card);
            if (!info) return;
            setContextTarget(card, info);
            const menuId = info.type === 'folder' ? 'folder-context-menu' : 'file-context-menu';
            const menu = document.getElementById(menuId);
            if (contextMenus && typeof contextMenus.syncFavoriteOptionLabels === 'function') {
                contextMenus.syncFavoriteOptionLabels();
            }
            if (contextMenus && typeof contextMenus.syncWopiOptionVisibility === 'function') {
                contextMenus.syncWopiOptionVisibility().catch(() => {});
            }
            if (contextMenus && typeof contextMenus.syncAddToPlaylistOption === 'function') {
                contextMenus.syncAddToPlaylistOption();
            }
            menu.style.left = `${e.pageX}px`;
            menu.style.top = `${e.pageY}px`;
            menu?.classList.remove('hidden');
        });

        // dragstart
        filesList.addEventListener('dragstart', (e) => {
            const card = e.target.closest('.file-item');
            if (!card) {
                e.preventDefault();
                return;
            }

            const info = itemInfo(card);
            if (!info) {
                e.preventDefault();
                return;
            }

            e.dataTransfer.setData('text/plain', info.id);
            if (info.type === 'folder') {
                e.dataTransfer.setData('application/oxicloud-folder', 'true');
            }
            // allow copy or move (handled by the browser)
            e.dataTransfer.effectAllowed = 'copyMove';

            this.draggedItems = document.createElement('div');
            this.draggedItems.className = 'dragged-items';

            let selectedCardFromList = filesList.querySelectorAll(`div.selected > div.name-cell`);
            if (selectedCardFromList.length === 0) {
                // fallback to current element
                selectedCardFromList = card.querySelectorAll('div.name-cell');
            }

            let index = 0;
            const maxElements = 4;
            let lastItemDiv = null;

            while (index < selectedCardFromList.length && index < maxElements) {
                const iconCell = document.createElement('div');
                const icon = selectedCardFromList[index].getElementsByClassName('file-icon').item(0)?.cloneNode(true);
                if (icon) {
                    iconCell.appendChild(icon);
                    iconCell.querySelectorAll('img')?.forEach((img) => {
                        img.loading = 'eager';
                    });
                }

                const nameCell = document.createElement('div');
                const name = selectedCardFromList[index].getElementsByTagName('span').item(0)?.cloneNode(true);
                if (name) {
                    nameCell.appendChild(name);
                }

                const div = document.createElement('div');
                div.className = 'file-item';
                div.appendChild(iconCell);
                div.appendChild(nameCell);

                this.draggedItems.appendChild(div);
                index += 1;
                lastItemDiv = div;
            }

            let downloadUrl;
            let nameEncoded;

            // tells Browser URL to call to drop selection on operating system (desktop, file manager etc)
            // will generate a zipfile if multiple
            if (selectedCardFromList.length === 1) {
                // only 1 file
                if (info.type === 'file') {
                    nameEncoded = info.name.replaceAll(/:/g, '-'); // issue is that DownloadURL is using : as separator;
                    downloadUrl = `${window.location.origin}/api/files/${info.id}`;
                } else {
                    // directory into ZIP
                    nameEncoded = info.name.replaceAll(/:/g, '-').concat('.zip');
                    downloadUrl = `${window.location.origin}/api/folders/${info.id}/download?format=zip`;
                }
            } else {
                // must use ZIP container
                // TODO better naming like ("selection in ${parent.name}") modulo i18n ? ...
                const now = new Date().toISOString().replace(/T/, ' ').replace(/\.*/, '').replaceAll(/:/g, '-');
                nameEncoded = `oxicloud ${now}.zip`;
                const folders = [];
                const files = [];
                filesList.querySelectorAll(`div.selected`).forEach((e) => {
                    const item = itemInfo(e);
                    if (item.type === 'file') {
                        files.push(item.id);
                    } else {
                        folders.push(item.id);
                    }
                });
                downloadUrl = `${window.location.origin}/api/batch/download?file_ids=${files.join(',')}&folder_ids=${folders.join(',')}`;
            }

            e.dataTransfer?.setData('DownloadURL', `application/octet-stream:${nameEncoded}:${downloadUrl}`);

            // if more than 1 item, display the badge
            if (selectedCardFromList.length > 1) {
                const badge = document.createElement('span');
                badge.className = 'dragged-items-badge';
                badge.innerText = `${selectedCardFromList.length}`;
                this.draggedItems.appendChild(badge);
            }

            // if more than maxElements display the fading
            if (selectedCardFromList.length > maxElements) {
                lastItemDiv.classList.add('fading');
            }

            this.dragPreview.appendChild(this.draggedItems);
            e.dataTransfer.setDragImage(this.draggedItems, 0, 0);
        });

        // dragend
        filesList.addEventListener('dragend', (_e) => {
            this.dragPreview.removeChild(this.draggedItems);
            document.querySelectorAll('.drop-target').forEach((el) => {
                el.classList.remove('drop-target');
            });
        });

        // dragover – only folders are valid drop targets
        filesList.addEventListener('dragover', (e) => {
            const card = e.target.closest('.file-item');
            if (!card || card.dataset.fileId) return;
            if (!card.dataset.folderId) return;
            e.preventDefault();
            card.classList.add('drop-target');
        });

        // dragleave
        filesList.addEventListener('dragleave', (e) => {
            const card = e.target.closest('.file-item');
            if (!card || card.dataset.fileId) return;
            card.classList.remove('drop-target');
        });

        // drop – only folders accept drops
        filesList.addEventListener('drop', async (e) => {
            const card = e.target.closest('.file-item');
            if (!card || card.dataset.fileId) return;
            const targetFolderId = card.dataset.folderId;
            if (!targetFolderId) return;

            e.preventDefault();
            card.classList.remove('drop-target');

            const action = e.dataTransfer.dropEffect;
            await this._dropToFolder(action, targetFolderId, e.dataTransfer);
        });
    },

    /* ================================================================
     *  Favorite star helper – attaches a direct click handler to a
     *  star <button> so the event never bubbles to the card.
     * ================================================================ */
    _bindStarClick(el) {
        const star = el.querySelector('.favorite-star');
        star?.addEventListener('click', (e) => {
            e.stopPropagation();
            e.stopImmediatePropagation();
            e.preventDefault();

            if (!favorites) return;

            // FIXME: make a function
            const itemElement = shared?.closest('.file-item');

            const itemId = itemElement.dataset.fileId ? itemElement.dataset.fileId : itemElement.dataset.folderId;
            const itemType = itemElement.dataset.fileId ? 'file' : 'folder';
            const itemName = itemElement.dataset.fileId ? itemElement.dataset.fileName : itemElement.dataset.folderName;

            const isActive = star.classList.contains('active');

            if (isActive) {
                this.setFavoriteVisualState(itemId, itemType, false);
                favorites.removeFromFavorites(itemId, itemType);
            } else {
                this.setFavoriteVisualState(itemId, itemType, true);
                favorites.addToFavorites(itemId, itemName, itemType);
            }

            // Keep context-menu label in sync if available
            if (contextMenus && typeof contextMenus.syncFavoriteOptionLabels === 'function') {
                contextMenus.syncFavoriteOptionLabels();
            }
        });

        const shared = el.querySelector('.file-badge-shared');
        shared?.addEventListener('click', (e) => {
            e.stopPropagation();
            e.stopImmediatePropagation();
            e.preventDefault();

            // FIXME: make a function
            const itemElement = shared?.closest('.file-item');

            const itemId = itemElement.dataset.fileId ? itemElement.dataset.fileId : itemElement.dataset.folderId;
            const itemType = itemElement.dataset.fileId ? 'file' : 'folder';
            const itemName = itemElement.dataset.fileId ? itemElement.dataset.fileName : itemElement.dataset.folderName;

            // TODO corrently dirty
            const item = {
                id: itemId,
                item_id: itemId,
                item_type: itemType,
                item_name: itemName
            };

            contextMenus.showShareDialog(item, itemType);
        });
    },

    /**
     * Sync favorite visuals for a file/folder across grid and list views.
     */
    setFavoriteVisualState(itemId, itemType, isFavorite) {
        const selector = itemType === 'folder' ? `#files-list .file-item[data-folder-id="${itemId}"]` : `#files-list .file-item[data-file-id="${itemId}"]`;

        const item = document.querySelector(selector);
        const starBtn = item?.querySelector('.favorite-star');

        // chzn
        if (starBtn) {
            starBtn.classList.toggle('active', !!isFavorite);

            // SVG icon path (after icons.js replacement)
            const svg = starBtn.querySelector('svg');
            const filledPath = OxiIcons?.star;
            const outlinePath = OxiIcons?.['star-outline'];
            const targetPath = isFavorite ? filledPath : outlinePath;
            if (svg && targetPath) {
                const p = svg.querySelector('path');
                if (p) p.setAttribute('d', targetPath[1]);
                svg.setAttribute('viewBox', `0 0 ${targetPath[0]} 512`);
            }

            // Fallback <i> icon (before icons.js replacement)
            const i = starBtn.querySelector('i');
            if (i) {
                i.classList.remove('fas', 'far');
                i.classList.add(isFavorite ? 'fas' : 'far');
            }
        }

        // toggle favorite's badge
        if (item) {
            const badgeFavorite = item.querySelector('.file-badge-favorite');
            badgeFavorite?.classList.toggle('hidden', !isFavorite);
        }
    },

    setSharedVisualState(itemId, itemType, isShared) {
        console.log(`setSharedVisual call for ${itemId} ${itemType} to ${isShared}`);
        const selector = itemType === 'folder' ? `#files-list .file-item[data-folder-id="${itemId}"]` : `#files-list .file-item[data-file-id="${itemId}"]`;
        // toggle favorite's badge
        const item = document.querySelector(selector);
        if (item) {
            const badgeShared = item.querySelector('.file-badge-shared');
            badgeShared?.classList.toggle('hidden', !isShared);
        }
    },

    /* ================================================================
     *  Element-creation helpers
     * ================================================================ */

    /** Create a list row for a folder */
    _createFolderItem(folder) {
        const el = document.createElement('div');
        el.className = 'file-item';
        el.dataset.folderId = folder.id;
        el.dataset.folderName = folder.name;
        el.dataset.parentId = folder.parent_id || '';

        const isFav = favorites?.isFavorite(folder.id, 'folder');
        const isShared = sharedView.isShared(folder.id, 'folder');
        const formattedDate = formatDateTime(folder.modified_at);

        el.innerHTML = `
            <div class="checkbox-cell"><input type="checkbox" class="item-checkbox"></div>
            <div class="name-cell">
                <div class="file-icon folder-icon">
                    <i class="fas fa-folder"></i>
                </div>
                <span>${escapeHtml(folder.name)}</span>
                <div class="file-badge file-badge-favorite ${isFav ? '' : 'hidden'}"><i class="fas fa-star favorite-star-inline"></i></div>
                <div class="file-badge file-badge-shared ${isShared ? '' : 'hidden'}"><i class="fas fa-share-alt"></i></div>
            </div>
            <div class="type-cell">${i18n.t('files.file_types.folder')}</div>
            <div class="size-cell">--</div>
            <div class="date-cell">${formattedDate}</div>
            <div class="action-cell">
                <button class="favorite-star${isFav ? ' active' : ''}">
                    <i class="${isFav ? 'fas' : 'far'} fa-star"></i>
                </button>
                <button class="file-actions"><i class="fas fa-ellipsis-v"></i></button>
            </div>
        `;

        if (app.currentPath !== '') {
            el.setAttribute('draggable', 'true');
        }
        this._bindStarClick(el);
        return el;
    },

    /** Create a grid card for a file */
    _createFileItem(file) {
        const iconClass = file.icon_class || this.getIconClass(file.name);
        const iconSpecialClass = file.icon_special_class || this.getIconSpecialClass(file.name);
        const cat = file.category || '';
        const typeLabel = cat ? i18n.t(`files.file_types.${cat.toLowerCase()}`) || cat : i18n.t('files.file_types.document');
        const fileSize = file.size_formatted || formatFileSize(file.size);
        const formattedDate = formatDateTime(file.modified_at);
        const isFav = favorites?.isFavorite(file.id, 'file');
        const isShared = sharedView.isShared(file.id, 'file');

        const el = document.createElement('div');
        el.className = 'file-item';
        el.dataset.fileId = file.id;
        el.dataset.fileName = file.name;
        el.dataset.folderId = file.folder_id || '';
        el.setAttribute('draggable', 'true');

        el.innerHTML = `

            <div class="checkbox-cell"><input type="checkbox" class="item-checkbox"></div>
            <div class="name-cell">
                <div class="file-icon ${iconSpecialClass}">
                    ${iconSpecialClass === 'image-icon' ? `<img class="file-thumb" src="/api/files/${file.id}/thumbnail/icon" loading="lazy" alt="">` : ''}
                    <i class="${iconClass}"></i>
                </div>
                <span>${escapeHtml(file.name)}</span>
                <div class="file-badge file-badge-favorite ${isFav ? '' : 'hidden'}"><i class="fas fa-star favorite-star-inline"></i></div>
                <div class="file-badge file-badge-shared ${isShared ? '' : 'hidden'}"><i class="fas fa-share-alt"></i></div>
            </div>
            <div class="type-cell">${typeLabel}</div>
            <div class="size-cell">${fileSize}</div>
            <div class="date-cell">${formattedDate}</div>
            <div class="action-cell">
                <button class="favorite-star${isFav ? ' active' : ''}">
                    <i class="${isFav ? 'fas' : 'far'} fa-star"></i>
                </button>
                <button class="file-actions"><i class="fas fa-ellipsis-v"></i></button>
            </div>
        `;
        var thumb = el.querySelector('.file-thumb');
        if (thumb)
            thumb.addEventListener('error', function () {
                this.style.display = 'none';
            });
        this._bindStarClick(el);
        return el;
    },

    /* ================================================================
     *  Batch rendering with DocumentFragment
     * ================================================================ */

    resetFilesList() {
        const filesList = document.getElementById('files-list');
        const filesContainerError = document.getElementById('files-container-error');

        if (!filesList) return;

        filesList.innerHTML = `
            <div class="list-header">
                <div class="list-header-checkbox"><input type="checkbox" id="select-all-checkbox" title="Select all"></div>
                <div data-i18n="files.name">Name</div>
                <div data-i18n="files.type">Type</div>
                <div data-i18n="files.size">Size</div>
                <div data-i18n="files.modified">Modified</div>
                <div></div><!-- actions -->
            </div>`;

        i18n.translateElement(filesList);

        filesList.classList.remove('hidden');
        filesContainerError?.classList.add('hidden');
    },

    showEmptyList() {
        this.showError(`
                <i class="fas fa-folder-open empty-state-icon"></i>
                <p data-i18n="files.no_files"></p>
                <p data-i18n="files.empty_hint"></p>
            `);
    },

    /**
     *
     * @param {string} content
     *
     */
    showError(content) {
        const filesContainerError = document.getElementById('files-container-error');
        const filesList = document.getElementById('files-list');
        if (filesContainerError) filesContainerError.innerHTML = content;

        i18n.translateElement(filesContainerError);

        filesContainerError?.classList.remove('hidden');
        filesList?.classList.add('hidden');
    },

    /**
     * Render an array of folders into both grid and list views
     * using DocumentFragment for minimal reflows.
     */
    renderFolders(folders) {
        if (!this._delegationReady) this.initDelegation();
        const safeFolders = Array.isArray(folders) ? folders : [];
        this._lastFolders = safeFolders.slice();

        for (const folder of safeFolders) {
            this._items.set(folder.id, folder);
        }

        this._renderFoldersToView(safeFolders);
    },

    /**
     * Render an array of files into both grid and list views
     * using DocumentFragment for minimal reflows.
     */
    renderFiles(files) {
        if (!this._delegationReady) this.initDelegation();
        const safeFiles = Array.isArray(files) ? files : [];
        this._lastFiles = safeFiles.slice();

        for (const file of safeFiles) {
            this._items.set(file.id, file);
        }

        this._renderFilesToView(safeFiles);
    },

    /* ================================================================
     *  Single-item add (backward-compatible API for post-upload, etc.)
     * ================================================================ */

    /**
     * Add a single folder to the active view.
     * @param {Object} folder - Folder object
     */
    addFolderToView(folder) {
        if (!this._delegationReady) this.initDelegation();

        // Duplicate guard
        if (document.querySelector(`.file-item[data-folder-id="${folder.id}"]`)) {
            console.log(`Folder ${folder.name} (${folder.id}) already exists in the view, not duplicating`);
            return;
        }

        this._items.set(folder.id, folder);
        this._upsertById(this._lastFolders, folder);
        this._renderFoldersToView([folder]);
    },

    /**
     * Add a single file to the active view.
     * @param {Object} file - File object
     */
    addFileToView(file) {
        if (!this._delegationReady) this.initDelegation();

        // Duplicate guard
        if (document.querySelector(`.file-item[data-file-id="${file.id}"]`)) {
            console.log(`File ${file.name} (${file.id}) already exists in the view, not duplicating`);
            return;
        }

        this._items.set(file.id, file);
        this._upsertById(this._lastFiles, file);
        this._renderFilesToView([file]);
    }
};

// --- Global helper functions for card interactions ---

/**
 * Toggle selection state of a file/folder card.
 * Routes through the multiSelect module so batch actions know about selected items.
 */
function toggleCardSelection(card, event) {
    if (multiSelect) {
        multiSelect.handleToggleItem(card, event);
    } else {
        card.classList.toggle('selected');
    }
}

/**
 * Show the context menu anchored next to a trigger element (the 3-dot button).
 */
function showContextMenuAtElement(triggerElement, menuId) {
    // Hide any open menus first
    document.querySelectorAll('.context-menu').forEach((m) => {
        m.classList.add('hidden');
    });

    const menu = document.getElementById(menuId);
    if (!menu) return;

    const rect = triggerElement.getBoundingClientRect();
    const menuWidth = 200; // approximate

    // Position below the trigger, aligned to the right edge
    let left = rect.right - menuWidth + window.scrollX;
    let top = rect.bottom + 4 + window.scrollY;

    // Keep inside viewport
    if (left < 8) left = 8;
    if (top + 300 > window.innerHeight + window.scrollY) {
        top = rect.top - 4 + window.scrollY; // flip above if no room
    }

    if (contextMenus && typeof contextMenus.syncFavoriteOptionLabels === 'function') {
        contextMenus.syncFavoriteOptionLabels();
    }
    if (contextMenus && typeof contextMenus.syncWopiOptionVisibility === 'function') {
        contextMenus.syncWopiOptionVisibility().catch(() => {});
    }
    if (contextMenus && typeof contextMenus.syncAddToPlaylistOption === 'function') {
        contextMenus.syncAddToPlaylistOption();
    }

    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;
    menu.classList.remove('hidden');
}

/**
 * Rubber band (lasso) selection — click + drag on empty grid area
 * to draw a rectangle and select all cards it touches.
 */
function initRubberBandSelection() {
    // Create the visual rectangle element once
    let selRect = document.getElementById('selection-rect');
    if (!selRect) {
        selRect = document.createElement('div');
        selRect.id = 'selection-rect';
        selRect.className = 'selection-rect';
        document.body.appendChild(selRect);
    }

    let active = false;
    let startX = 0,
        startY = 0;

    // We listen on the whole files-container (covers grid + empty space)
    const container = document.querySelector('.files-container') || document.getElementById('files-list');
    if (!container) return;

    container.addEventListener('mousedown', (e) => {
        // Only start if clicking empty area (not on a card, button, menu, input…)
        if (e.button !== 0) return; // left click only
        if (
            e.target.closest('.file-item') ||
            e.target.closest('.context-menu') ||
            e.target.closest('.upload-dropdown') ||
            e.target.closest('button') ||
            e.target.closest('input') ||
            e.target.closest('.breadcrumb') ||
            e.target.closest('.list-header')
        )
            return;

        active = true;
        startX = e.clientX;
        startY = e.clientY;

        selRect.style.left = `${startX}px`;
        selRect.style.top = `${startY}px`;
        selRect.style.width = '0px';
        selRect.style.height = '0px';
        selRect.style.display = 'none'; // show only after a small movement

        e.preventDefault(); // prevent text selection
    });

    document.addEventListener('mousemove', (e) => {
        if (!active) return;

        const curX = e.clientX;
        const curY = e.clientY;

        const left = Math.min(startX, curX);
        const top = Math.min(startY, curY);
        const width = Math.abs(curX - startX);
        const height = Math.abs(curY - startY);

        // Only show the rect after a small threshold to avoid flicker on click
        if (width > 5 || height > 5) {
            selRect.style.display = 'block';
        }

        selRect.style.left = `${left}px`;
        selRect.style.top = `${top}px`;
        selRect.style.width = `${width}px`;
        selRect.style.height = `${height}px`;

        // Highlight cards that intersect with the rectangle
        const rectBounds = { left, top, right: left + width, bottom: top + height };

        document.querySelectorAll('#files-list .file-item').forEach((card) => {
            const cardRect = card.getBoundingClientRect();
            const intersects =
                cardRect.left < rectBounds.right && cardRect.right > rectBounds.left && cardRect.top < rectBounds.bottom && cardRect.bottom > rectBounds.top;

            if (intersects) {
                card.classList.add('selected');

                // Sync with multiSelect module
                if (multiSelect) {
                    const info = multiSelect._extractInfo(card);
                    if (info) multiSelect.select(info.id, info.name, info.type, info.parentId);
                }
            } else {
                card.classList.remove('selected');
                // Deselect from multiSelect module
                if (multiSelect) {
                    const info = multiSelect._extractInfo(card);
                    if (info) multiSelect.deselect(info.id);
                }
            }
        });
    });

    document.addEventListener('mouseup', () => {
        if (!active) return;
        active = false;
        const hadSelection = selRect.style.display === 'block';
        selRect.style.display = 'none';
        // Update the batch bar after rubber band selection completes
        if (multiSelect) multiSelect._syncUI();
        // Suppress the click event that follows mouseup so the global
        // deselect handler doesn't immediately clear the selection.
        if (hadSelection) {
            __rubberBandJustFinished = true;
            requestAnimationFrame(() => {
                __rubberBandJustFinished = false;
            });
        }
    });
}

// Initialize rubber band once DOM is ready
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initRubberBandSelection);
} else {
    initRubberBandSelection();
}

/**
 * Show a modern confirm dialog (replaces native confirm())
 * @param {Object} options
 * @param {string} options.title - Dialog title
 * @param {string} options.message - Dialog message/body
 * @param {string} [options.confirmText='Confirmar'] - Text for confirm button
 * @param {string} [options.cancelText='Cancelar'] - Text for cancel button
 * @param {boolean} [options.danger=false] - Use danger styling (red)
 * @returns {Promise<boolean>} true if confirmed, false if cancelled
 */
function showConfirmDialog({ title, message, confirmText, cancelText, danger = true } = {}) {
    const ct = confirmText || i18n.t('actions.delete');
    const cc = cancelText || i18n.t('actions.cancel');
    const t = title || i18n.t('dialogs.confirm_title');

    return new Promise((resolve) => {
        // Remove any previous confirm dialog
        const prev = document.getElementById('confirm-dialog-overlay');
        if (prev) prev.remove();

        const overlay = document.createElement('div');
        overlay.id = 'confirm-dialog-overlay';
        overlay.className = 'confirm-dialog';
        overlay.innerHTML = `
            <div class="confirm-dialog-content">
                <div class="confirm-dialog-icon">
                    <i class="fas ${danger ? 'fa-exclamation-triangle' : 'fa-question-circle'}"></i>
                </div>
                <div class="confirm-dialog-title">${t}</div>
                <div class="confirm-dialog-message">${message || ''}</div>
                <div class="confirm-dialog-buttons">
                    <button class="btn btn-secondary confirm-dialog-cancel">${cc}</button>
                    <button class="btn ${danger ? 'btn-danger' : 'btn-primary'} confirm-dialog-ok">${ct}</button>
                </div>
            </div>
        `;
        document.body.appendChild(overlay);

        // Force layout then show
        requestAnimationFrame(() => {
            overlay.classList.add('active');
        });

        const cleanup = (result) => {
            overlay.classList.remove('active');
            setTimeout(() => overlay.remove(), 200);
            resolve(result);
        };

        overlay.querySelector('.confirm-dialog-cancel').addEventListener('click', () => cleanup(false));
        overlay.querySelector('.confirm-dialog-ok').addEventListener('click', () => cleanup(true));
        overlay.addEventListener('click', (e) => {
            if (e.target === overlay) cleanup(false);
        });
    });
}

export { initRubberBandSelection, showConfirmDialog, showContextMenuAtElement, toggleCardSelection, ui };
