/**
 * OxiCloud - App state container
 * Centralized mutable state for app and cached DOM references.
 */

/** @import {FolderInfo} from '../core/types.js' */

export const app = {
    currentView: 'grid',

    /** @type {string | null} */
    currentPath: '',
    currentFolder: null,

    /** @type {FolderInfo | null} */
    currentFolderInfo: null,

    /** @type {Object | null} */
    contextMenuTargetFolder: null,

    /** @type {Object | null} */
    contextMenuTargetFile: null,
    selectedTargetFolderId: '',
    moveDialogMode: 'file',

    /** @type {String | null} */
    currentSection: null, // will be defined on first call
    isSearchMode: false,
    shareDialogItem: null,
    shareDialogItemType: null,
    notificationShareUrl: null,
    userHomeFolderId: null,
    userHomeFolderName: null,
    /** @type {Object[]} */
    breadcrumbPath: [], // Array of {id, name} tracking folder navigation hierarchy

    /** @type {String | null} */
    viewFile: null // current file in inline view
};

export const appElements = {
    /** @type {HTMLElement | null} */
    uploadBtn: null,
    /** @type {HTMLElement | null}  */
    dropzone: null,
    /** @type {HTMLInputElement | null}  */
    fileInput: null,
    /** @type {HTMLElement | null}  */
    filesList: null,
    /** @type {HTMLElement | null}  */
    newFolderBtn: null,
    /** @type {HTMLElement | null}  */
    gridViewBtn: null,
    /** @type {HTMLElement | null}  */
    listViewBtn: null,
    /** @type {HTMLElement | null}  */
    breadcrumb: null,
    /** @type {HTMLElement | null}  */
    pageTitle: null,
    /** @type {HTMLElement | null}  */
    actionsBar: null,
    /** @type {NodeListOf<HTMLElement> | null}  */
    navItems: null,
    /** @type {HTMLInputElement | null}  */
    searchInput: null
};
