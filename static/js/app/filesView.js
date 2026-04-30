// @ts-check

import { i18n } from '../core/i18n.js';
import { inlineViewer } from '../features/files/inlineViewer.js';
import { multiSelect } from '../features/files/multiSelect.js';
import { resolveHomeFolder } from './authSession.js';
import { updateHistory } from './main.js';
import { app } from './state.js';
import { ui } from './ui.js';
import { uiNotifications } from './uiNotifications.js';

/** @import {FileInfo, FolderInfo} from '../core/types.js' */

let isLoadingFiles = false;

/**
 * getFolder information
 * @param {string} id the id of the folder
 * @returns {Promise<FolderInfo>}
 */
async function getFolder(id) {
    /** @type {HeadersInit} */
    const headers = {
        'Cache-Control': 'no-cache, no-store, must-revalidate',
        Pragma: 'no-cache'
    };

    /** @type {RequestInit} */
    const requestOptions = {
        headers,
        credentials: 'same-origin',
        cache: 'no-store'
    };

    const folderInformations = await fetch(`/api/folders/${id}`, requestOptions);
    if (folderInformations.ok) {
        return folderInformations.json();
    } else {
        console.warn(`Error fetching folder ${id}`);
        return Promise.reject(null);
    }
}

/**
 * rebuild breadcrumb from selected folder (iterate up to root)
 */
async function rebuildBreadCrumb() {
    /**
     * Store the leaf (this is the current displayed folder)
     * @type {FolderInfo | null}
     */
    let currentFolderInfo = null;

    // rebuild full breadcrumb,
    // TODO: to optimize, data may already be known / or ETAG could be interesting to reduce load
    app.breadcrumbPath = [];

    /** @type {string | null} */
    let id = app.currentPath;

    // recurse from selected folder to root
    while (id !== null) {
        console.log(`fetching folder information for folder ${id}`);
        try {
            const folderInfo = await getFolder(id);

            // store the Leaf which is the current folder
            if (currentFolderInfo === null) {
                currentFolderInfo = folderInfo;
            }

            // XXX do not enter root into bread crumb updateBreadcrumb() method always display it
            if (!folderInfo.is_root) {
                app.breadcrumbPath.unshift({
                    id: folderInfo.id,
                    name: folderInfo.name
                });
            }

            // iterate to parent folder
            id = folderInfo.parent_id;
        } catch (_e) {
            console.log(`Error loading information from folder ${app.currentPath}, falling back to ${app.userHomeFolderId}`);
            // fallback of root
            uiNotifications.show('error: folder not found or permission denied', 'the given folder is not available or you do not have sufficient rights');
            app.breadcrumbPath = [];
            id = app.userHomeFolderId;
            if (id) app.currentPath = id;
        }
    }

    // store informations on the current folder
    app.currentFolderInfo = currentFolderInfo;
}

// TODO split load() vs view()
/**
 * Files view loading logic
 *
 * @param {Object} options
 * @param {boolean} [options.insertHistory] add browser history (default true)
 * @param {boolean} [options.forceRefresh] force refresh of content
 *
 */
async function loadFiles(options = { insertHistory: true }) {
    try {
        console.log('Starting loadFiles() - loading files...', options);

        const forceRefresh = options.forceRefresh || false;

        if (isLoadingFiles) {
            console.log('A file load is already in progress, ignoring request');
            return;
        }

        isLoadingFiles = true;

        // This to avoid blinking page, a better solution would be to put loading on an overlay and remove timeout
        const loadingFiles = setTimeout(() => {
            // display loader after few delay (will be canceled if result take less time)
            ui.showError(`
                <div class="files-loading-spinner">
                    <div class="spinner"></div>
                    <span>${i18n.t('files.loading')}</span>
                </div>
            `);
        }, 100);

        if (!app.userHomeFolderId) {
            await resolveHomeFolder();
        }

        const timestamp = Math.floor(Date.now() / 1000);

        await rebuildBreadCrumb();

        // request a breadcrumb paint
        ui.updateBreadcrumb();

        updateHistory(options.insertHistory || false);

        let url;

        if (!app.currentPath || app.currentPath === '') {
            if (app.userHomeFolderId) {
                url = `/api/folders/${app.userHomeFolderId}/listing?t=${timestamp}`;
                app.currentPath = app.userHomeFolderId;
                app.breadcrumbPath = [];
                ui.updateBreadcrumb();
                console.log(`Loading user folder: ${app.userHomeFolderName} (${app.userHomeFolderId})`);
            } else {
                url = `/api/folders?t=${timestamp}`;
                console.warn('Emergency fallback to root folder - this should not normally happen');
            }
        } else {
            url = `/api/folders/${app.currentPath}/listing?t=${timestamp}`;
            console.log(`Loading subfolder content: ${app.currentPath}`);
        }

        /** @type {HeadersInit} */
        const headers = {
            'Cache-Control': 'no-cache, no-store, must-revalidate',
            Pragma: 'no-cache'
        };

        /** @type {RequestInit} */
        const requestOptions = {
            headers,
            credentials: 'same-origin',
            cache: 'no-store'
        };

        if (forceRefresh) {
            url += `&force_refresh=true`;
            if (requestOptions.headers) requestOptions.headers['X-Force-Refresh'] = 'true';
            console.log('Forcing complete refresh ignoring cache');
        }

        console.log(`Loading listing from ${url}`);
        const response = await fetch(url, requestOptions);

        // not required anymore
        clearTimeout(loadingFiles);

        if (response.status === 401 || response.status === 403) {
            console.warn('Auth error when loading files, showing empty list');
            // FIXME: i18n
            ui.showError(`<p>Could not load files</p>`);
            return;
        }

        if (!response.ok) {
            throw new Error(`Server responded with status: ${response.status}`);
        }

        const listing = await response.json();

        ui._items.clear();
        ui.resetFilesList();
        if (multiSelect) {
            multiSelect.clear();
            multiSelect.init(); // this will wire buttons & select-all-checkbox
        }

        /** @type {FolderInfo[]} */
        const folderList = Array.isArray(listing.folders) ? listing.folders : [];

        /** @type {FileInfo[]} */
        const fileList = Array.isArray(listing.files) ? listing.files : [];

        if (folderList.length === 0 && fileList.length === 0) {
            ui.showEmptyList();
        } else {
            ui.renderFolders(folderList);
            ui.renderFiles(fileList);

            // check if a file was provided
            if (app.viewFile) {
                let fileFound = null;

                // lookup for the given fle
                for (const file of fileList) {
                    if (file.id === app.viewFile) {
                        fileFound = file;
                        break;
                    }
                }

                if (fileFound) {
                    console.log(`file ${app.viewFile} found, calling viewer`);
                    await inlineViewer.openFile(fileFound);
                } else {
                    // remove file
                    console.log(`file ${app.viewFile} not found`);
                    app.viewFile = null;

                    // correct url/history as file is not found
                    updateHistory(false);
                }
            }
        }

        console.log(`Loaded ${folderList.length} folders and ${fileList.length} files`);
    } catch (error) {
        console.error('Error loading folders:', error);
        ui.showNotification('Error', 'Could not load files and folders');
    } finally {
        isLoadingFiles = false;
    }
}

export { loadFiles };
