/**
 * OxiCloud - Main Application
 * This file contains the core functionality, initialization and state management
 */

import { formatFileSize, formatQuotaSize } from '../core/formatters.js';
import { i18n } from '../core/i18n.js';
import { Modal } from '../core/modal.js';
import { fileOps } from '../features/files/fileOperations.js';
import { multiSelect } from '../features/files/multiSelect.js';
import { favorites } from '../features/library/favorites.js';
import { recent } from '../features/library/recent.js';
import { fileSharing } from '../features/sharing/fileSharing.js';
import { sharedView } from '../views/shared/sharedView.js';
import { checkAuthentication } from './authSession.js';
import { loadFiles } from './filesView.js';
import {
    SECTIONS_MAPPER,
    switchToFavoritesSection,
    switchToFilesSection,
    switchToMusicSection,
    switchToPhotosSection,
    switchToRecentFilesSection,
    switchToSharedSection,
    switchToTrashSection
} from './navigation.js';
import { performSearch } from './searchView.js';
import { app, appElements as elements } from './state.js';
import { loadTrashItems } from './trashView.js';
import { ui } from './ui.js';
import { setupUserMenu } from './userMenu.js';

// Upload dropdown listener state (prevents accumulated listeners)
/** @type {((e: MouseEvent) => void) | null} */
let uploadDropdownDocumentClickHandler = null;

/** @type { AbortController | null } */
let uploadDropdownBindingsController = null;
let actionsBarDelegationBound = false;

const _multiSelectButons = `
    <div class="action-buttons batch-selection-bar hidden" id="multi-select-buttons">
        <div class="list-header-checkbox">
            <button class="batch-bar-close" id="batch-selection-close" title="Cancel selection">
                    <i class="fas fa-times"></i>
            </button>
            <span class="batch-bar-count" id="batch-bar-count"></span>
        </div>
        <div class="batch-selection-info">
            <div class="batch-bar-actions">
                <button class="batch-btn" id="batch-fav" title="Add to favorites" data-i18n-title="batch.add_favorites">
                    <i class="fas fa-star"></i>
                    <span data-i18n="batch.add_favorites">Add to favorites</span>
                </button>
                <button class="batch-btn" id="batch-move" title="Move or copy" data-i18n-title="batch.move_copy">
                    <i class="fas fa-arrows-alt"></i>
                    <span data-i18n="batch.move_copy">Move or copy</span>
                </button>
                <button class="batch-btn" id="batch-download" title="Download" data-i18n-title="actions.download">
                    <i class="fas fa-download"></i>
                    <span data-i18n="actions.download">Download</span>
                </button>
                <button class="batch-btn batch-btn-danger" id="batch-delete" title="Delete" data-i18-title="actions.delete">
                    <i class="fas fa-trash-alt"></i>
                    <span data-i18n="actions.delete">Delete</span>
                </button>
            </div>
        </div>
    </div>
`;

const _toggleButtons = `
    <div class="view-toggle">
        <button class="toggle-btn active" id="grid-view-btn" title="Grid view">
            <i class="fas fa-th"></i>
        </button>
        <button class="toggle-btn" id="list-view-btn" title="List view">
            <i class="fas fa-list"></i>
        </button>
    </div>
`;

const ACTIONS_BAR_TEMPLATES = {
    files: `
        <div class="action-buttons" id="default-buttons">
            <div class="upload-dropdown" id="upload-dropdown">
                <button class="btn btn-primary" id="upload-btn">
                    <i class="fas fa-cloud-upload-alt icon-mr"></i>
                    <span data-i18n="actions.upload">Upload</span>
                    <i class="fas fa-caret-down icon-ml"></i>
                </button>
                <div class="upload-dropdown-menu hidden" id="upload-dropdown-menu">
                    <button class="upload-dropdown-item" id="upload-files-btn">
                        <i class="fas fa-file"></i>
                        <span data-i18n="actions.upload_files">Upload files</span>
                    </button>
                    <button class="upload-dropdown-item" id="upload-folder-btn">
                        <i class="fas fa-folder-open"></i>
                        <span data-i18n="actions.upload_folder">Upload folder</span>
                    </button>
                </div>
            </div>
            <button class="btn btn-secondary" id="new-folder-btn">
                <i class="fas fa-folder-plus icon-mr"></i>
                <span data-i18n="actions.new_folder">New folder</span>
            </button>
        </div>
        ${_multiSelectButons}
        ${_toggleButtons}
    `,
    trash: `
        <div class="action-buttons" id="default-buttons">
            <button class="btn btn-danger" id="empty-trash-btn">
                <i class="fas fa-trash-alt"></i>
                <span data-i18n="trash.empty_trash">Empty trash</span>
            </button>
        </div>
        ${_toggleButtons}
    `,
    favorites: `
        <div class="action-buttons" id="default-buttons"></div>
        ${_multiSelectButons}
        ${_toggleButtons}
    `,
    recent: `
        <div class="action-buttons" id="default-buttons">
            <button class="btn btn-secondary" id="clear-recent-btn">
                <i class="fas fa-broom icon-mr"></i>
                <span data-i18n="actions.clear_recent">Clear recent</span>
            </button>
        </div>
        ${_multiSelectButons}
        ${_toggleButtons}
    `
};

/**
 *
 * @param {string} mode
 * @param {boolean} [force=false]
 * @returns
 */
function setActionsBarMode(mode, force = false) {
    if (!elements.actionsBar) return;

    if (mode === 'hidden') {
        elements.actionsBar.classList.add('hidden');
        elements.actionsBar.dataset.mode = 'hidden';
        return;
    }

    if (!force && elements.actionsBar.dataset.mode === mode) {
        return;
    }

    const html = ACTIONS_BAR_TEMPLATES[mode];
    if (!html) return;

    elements.actionsBar.innerHTML = html;
    elements.actionsBar.classList.remove('hidden');
    elements.actionsBar.dataset.mode = mode;

    // Refresh cached action elements after rebuild
    elements.uploadBtn = document.getElementById('upload-btn');
    elements.newFolderBtn = document.getElementById('new-folder-btn');
    elements.gridViewBtn = document.getElementById('grid-view-btn');
    elements.listViewBtn = document.getElementById('list-view-btn');

    i18n.translateElement(elements.actionsBar);

    if (mode === 'files') {
        setupUploadDropdown();
    }
}

function setupActionsBarDelegation() {
    if (actionsBarDelegationBound || !elements.actionsBar) return;
    actionsBarDelegationBound = true;

    elements.actionsBar.addEventListener('click', async (e) => {
        const btn = /** @type {HTMLElement} */ (e.target)?.closest('button');
        if (!btn) return;

        switch (btn.id) {
            case 'upload-files-btn': {
                e.stopPropagation();
                const menu = document.getElementById('upload-dropdown-menu');
                if (menu) menu.classList.add('hidden');
                if (elements.fileInput) elements.fileInput.click();
                break;
            }
            case 'upload-folder-btn': {
                e.stopPropagation();
                const menu = document.getElementById('upload-dropdown-menu');
                if (menu) menu.classList.add('hidden');
                const folderInput = document.getElementById('folder-input');
                if (folderInput) folderInput.click();
                break;
            }
            case 'new-folder-btn': {
                const folderName = await Modal.promptNewFolder();
                if (folderName) {
                    fileOps.createFolder(folderName);
                }
                break;
            }
            case 'grid-view-btn':
                ui.switchToGridView();
                break;
            case 'list-view-btn':
                ui.switchToListView();
                break;
            case 'empty-trash-btn':
                if (await fileOps.emptyTrash()) {
                    loadTrashItems();
                }
                break;
            case 'clear-recent-btn':
                if (recent) {
                    recent.clearRecentFiles();
                    recent.displayRecentFiles();
                    ui.showNotification('Cleanup completed', 'Recent files history has been cleared');
                }
                break;
            default:
                break;
        }
    });
}

/**
 * @typedef {Object} OxiContext
 * @property {string} section
 * @property {string | null} path the uuid of the path
 * @property {string | null} file the uuid of file inline view
 */

/**
 * Read the application hash
 *
 * format:
 *
 * #/<section>/
 *
 * #/shared
 * #/recent
 * ...
 *
 * special case of drive:
 *
 * #/files/folder/<folder ID>
 *
 * @returns {OxiContext}
 */
function deserializeHash() {
    const hashContext = /** type {OxiContext} */ {};

    // FIXME rename files into drive ?
    hashContext.section = 'files';

    const hash_elements = window.location.hash.split('/');

    const section = hash_elements[1];

    if (section in SECTIONS_MAPPER) {
        hashContext.section = section;
    }

    if (hash_elements[1] === 'files' && hash_elements[2] === 'folder' && hash_elements[3] !== null) {
        hashContext.path = hash_elements[3];

        if (hash_elements[4] === 'file' && hash_elements[5] !== null) {
            hashContext.file = hash_elements[5];
        }
    }

    return hashContext;
}

/**
 * update borwser's url/history
 *
 * @param {boolean} insertHistory true to change url and browser's history, false to change url only
 */
function updateHistory(insertHistory) {
    const historyData = {
        section: app.currentSection,
        id: app.currentFolder,
        file: app.viewFile
    };

    let historyUrl = `#/${app.currentSection}`;

    if (app.currentSection === 'files' && app.currentFolderInfo !== null) {
        historyData.id = app.currentFolder;
        historyUrl = historyUrl.concat('/folder/', app.currentFolderInfo.id);

        if (app.viewFile) {
            historyUrl = historyUrl.concat('/file/', app.viewFile);
        }
        // update title
        document.title = `OxiCloud: ${app.currentFolderInfo.path}`;
    }

    if (insertHistory) {
        console.log(`adding history with ${historyUrl}`);
        window.history.pushState(historyData, '', historyUrl);
    } else {
        console.log(`replace history with ${historyUrl}`);
        window.history.replaceState(historyData, '', historyUrl);
    }
}

/**
 *
 * @param {string} section
 * @returns
 */
function switchSectionTo(section) {
    if (app.currentSection === section)
        // no change ...
        return;

    if (!(section in SECTIONS_MAPPER)) {
        console.warn(`context view ${section} unkonwn fallback to files section`);
        section = 'files';
    }

    const switchHandler = SECTIONS_MAPPER[section];
    switchHandler();
}

/**
 * Initialize the application
 */
function initApp() {
    // Cache DOM elements
    cacheElements();

    // Initialize file sharing module first
    if (fileSharing?.init) {
        fileSharing.init();
    } else {
        console.warn('fileSharing module not fully initialized');
    }

    // Then create menus and dialogs after modules have initialized
    setTimeout(() => {
        ui.initializeContextMenus();
    }, 100);

    // Setup event listeners
    setupEventListeners();

    // Initialize favorites module if available
    if (favorites?.init) {
        console.log('Initializing favorites module');
        favorites.init();
    } else {
        console.warn('Favorites module not available or not initializable');
    }

    // Initialize recent files module if available
    if (recent?.init) {
        console.log('Initializing recent files module');
        recent.init();
    } else {
        console.warn('Recent files module not available or not initializable');
    }

    // Initialize multi-select / batch actions
    if (multiSelect?.init) {
        console.log('Initializing multi-select module');
        multiSelect.init();
    }

    window.addEventListener('authenticationDone', async () => {
        // Check if a context was provided in the URL
        const hashContext = deserializeHash();
        switchSectionTo(hashContext.section);
        if (hashContext.section === 'files') {
            if (hashContext.path) {
                console.log(`init: reusing folder from hash URL: ${hashContext.path}`);
                app.currentPath = hashContext.path;
            }

            if (hashContext.file !== null) {
                app.viewFile = hashContext.file;
            }

            loadFiles();
        }
    });

    // Wait for translations to load before checking authentication
    if (i18n.isLoaded()) {
        // Translations already loaded, proceed with authentication
        checkAuthentication();
    } else {
        // Wait for translations to be loaded before proceeding
        console.log('Waiting for translations to load...');
        window.addEventListener('translationsLoaded', () => {
            console.log('Translations loaded, proceeding with authentication');
            checkAuthentication();
        });

        // Set a timeout as a fallback in case translations take too long
        setTimeout(() => {
            if (!i18n.isLoaded()) {
                console.warn('Translations loading timeout, proceeding with authentication anyway');
                checkAuthentication();
            }
        }, 3000); // 3 second timeout
    }
}

/**
 * Cache DOM elements for faster access
 */
function cacheElements() {
    elements.uploadBtn = document.getElementById('upload-btn');
    elements.dropzone = document.getElementById('dropzone');
    elements.fileInput = /** @type {HTMLInputElement} */ (document.getElementById('file-input'));
    elements.filesList = document.getElementById('files-list');
    elements.newFolderBtn = document.getElementById('new-folder-btn');
    elements.gridViewBtn = document.getElementById('grid-view-btn');
    elements.listViewBtn = document.getElementById('list-view-btn');
    elements.breadcrumb = document.querySelector('.breadcrumb');
    elements.pageTitle = document.querySelector('.page-title');
    elements.actionsBar = document.getElementById('actions-bar');
    elements.navItems = document.querySelectorAll('.nav-item');
    elements.searchInput = document.querySelector('.search-container input');
}

/**
 * Setup the upload dropdown button and menu
 * Handles opening/closing the dropdown and triggering file/folder inputs
 */
function setupUploadDropdown() {
    const uploadBtn = document.getElementById('upload-btn');
    const menu = document.getElementById('upload-dropdown-menu');

    if (!uploadBtn || !menu) return;

    // Abort any previous local bindings (safe across repeated/rebuilt UI)
    if (uploadDropdownBindingsController) {
        uploadDropdownBindingsController.abort();
    }
    uploadDropdownBindingsController = new AbortController();
    const signal = uploadDropdownBindingsController.signal;

    // Toggle dropdown on button click
    uploadBtn.addEventListener(
        'click',
        (e) => {
            e.stopPropagation();
            const isOpen = !menu.classList.contains('hidden');
            // Close any other open dropdowns
            document.querySelectorAll('.upload-dropdown-menu').forEach((m) => {
                m.classList.add('hidden');
            });
            if (!isOpen) {
                menu.classList.remove('hidden');
            }
        },
        { signal }
    );

    // Close dropdown when clicking outside
    // remove+add stable handler: guarantees exactly one global listener
    if (uploadDropdownDocumentClickHandler) {
        document.removeEventListener('click', uploadDropdownDocumentClickHandler);
    }
    uploadDropdownDocumentClickHandler = (e) => {
        if (/** @type {HTMLElement} */ (e.target)?.closest('#upload-dropdown')) return;
        document.querySelectorAll('.upload-dropdown-menu').forEach((m) => {
            m.classList.add('hidden');
        });
    };
    document.addEventListener('click', uploadDropdownDocumentClickHandler);
}

/**
 * Setup event listeners for main UI elements
 */
function setupEventListeners() {
    // Set up drag and drop
    ui.setupDragAndDrop();

    // Debounce timer for live search
    let searchDebounceTimer = null;
    const SEARCH_DEBOUNCE_MS = 300;
    const SEARCH_MIN_CHARS = 3;

    // handle history / url change
    window.addEventListener('popstate', (e) => {
        if (e.state === null) {
            // change is from user (url explicitely change, read information from hash)
            const hashContext = deserializeHash();
            switchSectionTo(hashContext.section);
            if (hashContext.path) {
                app.currentPath = hashContext.path;
                loadFiles({ insertHistory: false });
            }
        } else {
            // change is from history, data provided in event
            switchSectionTo(e.state.section);
            app.currentPath = e.state.id;
            loadFiles({ insertHistory: false });
        }
    });

    // Search input — Enter key
    elements.searchInput?.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
            // Cancel any pending debounce
            if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
            const query = elements.searchInput?.value.trim();

            // In shared section, filter locally
            if (app.currentSection === 'shared' && sharedView) {
                sharedView.filterAndSortItems();
                return;
            }

            if (query) {
                performSearch(query);
            } else if (app.isSearchMode) {
                // If search is empty and we're in search mode, return to normal view
                app.isSearchMode = false;
                app.currentPath = '';
                ui.updateBreadcrumb();
                loadFiles();
            }
        }
    });

    // Search input — Live search (debounced, after 3+ chars)
    elements.searchInput?.addEventListener('input', () => {
        if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
        const query = elements.searchInput?.value.trim();
        if (!query) return;

        if (query.length >= SEARCH_MIN_CHARS) {
            searchDebounceTimer = setTimeout(() => {
                performSearch(query);
            }, SEARCH_DEBOUNCE_MS);
        } else if (query.length === 0 && app.isSearchMode) {
            // User cleared the search input — return to normal view
            searchDebounceTimer = setTimeout(() => {
                app.isSearchMode = false;
                app.currentPath = '';
                ui.updateBreadcrumb();
                loadFiles();
            }, SEARCH_DEBOUNCE_MS);
        }
    });

    // Search button
    document.getElementById('search-button')?.addEventListener('click', () => {
        if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
        const query = elements.searchInput?.value.trim();
        if (query) {
            performSearch(query);
        }
    });

    // Upload dropdown
    setupUploadDropdown();
    setupActionsBarDelegation();
    if (elements.actionsBar) {
        elements.actionsBar.dataset.mode = 'files';
    }

    // File input
    elements.fileInput?.addEventListener('change', (e) => {
        const target = /** @type {HTMLInputElement} */ (e.target);
        if (!target) return;
        if (!target.files) return;
        if (target.files.length > 0) {
            fileOps.uploadFiles(target.files);
            target.value = ''; // reset so same file can be re-uploaded
        }
    });

    // Folder input
    const folderInput = document.getElementById('folder-input');
    if (folderInput) {
        folderInput.addEventListener('change', (e) => {
            const target = /** @type {HTMLInputElement} */ (e.target);
            if (!target) return;
            if (!target.files) return;
            if (target.files.length > 0) {
                fileOps.uploadFolderFiles(target.files);
                target.value = '';
            }
        });
    }

    // Sidebar navigation
    elements.navItems?.forEach((item) => {
        item.addEventListener('click', () => {
            // Remove active class from all nav items
            elements.navItems?.forEach((navItem) => {
                navItem.classList.remove('active');
            });

            // Add active class to clicked item
            item.classList.add('active');
            let _updateHistory = true;

            const itemI18nKey = item.querySelector('span')?.getAttribute('data-i18n');

            switch (itemI18nKey) {
                case 'nav.shared':
                    // Switch to shared view
                    switchToSharedSection();
                    break;

                case 'nav.favorites':
                    // Switch to favorites view
                    switchToFavoritesSection();
                    break;

                case 'nav.recent':
                    // Switch to recent files view
                    switchToRecentFilesSection();
                    break;

                case 'nav.photos':
                    switchToPhotosSection();
                    break;

                case 'nav.music':
                    switchToMusicSection();
                    break;

                case 'nav.trash':
                    switchToTrashSection();
                    break;

                default:
                    // Use the proper switchToFilesView function which handles all UI restoration
                    switchToFilesSection();
                    // FIXME: because fileview handles it: need to converge code
                    _updateHistory = false;
            }

            document.title = `OxiCloud: ${i18n.t(itemI18nKey)}`;

            if (_updateHistory) {
                updateHistory(true);
            }
        });
    });

    // Load saved view preference
    const savedView = localStorage.getItem('oxicloud-view');
    if (savedView === 'list') {
        ui.switchToListView();
    } else {
        ui.switchToGridView();
    }

    // User menu
    setupUserMenu();

    // Global events to close context menus and deselect cards
    document.addEventListener('click', (e) => {
        const folderMenu = document.getElementById('folder-context-menu');
        const target = /** @type {HTMLElement} */ (e.target);
        if (folderMenu && !folderMenu.classList.contains('hidden') && !folderMenu.contains(target)) {
            ui.closeContextMenu();
        }

        const fileMenu = document.getElementById('file-context-menu');
        if (fileMenu && !fileMenu.classList.contains('hidden') && !fileMenu.contains(target)) {
            ui.closeFileContextMenu();
        }
    });
}

// View-switching actions moved to app/navigation.js

/**
 * Navigate into a folder and refresh the file list.
 * @param {string} id
 * @param {string} name
 */
export function selectFolder(id, name) {
    app.breadcrumbPath.push({ id, name });
    app.currentPath = id;
    ui.updateBreadcrumb();
    loadFiles();
}

/**
 * Update the storage usage display with the user's actual storage usage
 * @param {Object} userData - The user data object
 */
function updateStorageUsageDisplay(userData) {
    // Default values
    const DEFAULT_QUOTA = 10 * 1024 * 1024 * 1024; // 10 GB
    let usedBytes = 0;
    let quotaBytes = DEFAULT_QUOTA;
    let usagePercentage = 0;

    // Get values from user data if available
    if (userData) {
        usedBytes = userData.storage_used_bytes || 0;
        // Use == null to allow 0 (unlimited) to pass through; only default to DEFAULT_QUOTA when null/undefined
        quotaBytes = userData.storage_quota_bytes == null ? DEFAULT_QUOTA : userData.storage_quota_bytes;

        // Calculate percentage (avoid division by zero)
        if (quotaBytes > 0) {
            usagePercentage = Math.min(Math.round((usedBytes / quotaBytes) * 100), 100);
        }
    }

    // Format the numbers for display
    const usedFormatted = formatFileSize(usedBytes);
    const quotaFormatted = formatQuotaSize(quotaBytes);

    // Update the storage display elements
    const storageFill = /** @type {HTMLDivElement} */ (document.querySelector('.storage-fill'));
    const storageInfo = /** @type {HTMLDivElement} */ (document.querySelector('.storage-info'));

    if (storageFill) {
        storageFill.style.width = `${usagePercentage}%`;
    }

    if (storageInfo) {
        // Remove data-i18n attribute to prevent i18n from overwriting our value
        storageInfo.removeAttribute('data-i18n');

        storageInfo.textContent = i18n.t('storage.used', {
            percentage: usagePercentage,
            used: usedFormatted,
            total: quotaFormatted
        });
    }

    console.log(`Updated storage display: ${usagePercentage}% (${usedFormatted} / ${quotaFormatted})`);
}

export { deserializeHash, initApp, setActionsBarMode, updateHistory, updateStorageUsageDisplay };
