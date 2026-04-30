/**
 * OxiCloud - View navigation actions
 * Extracted from main.js to keep navigation concerns isolated.
 */

import { i18n } from '../core/i18n.js';
import { multiSelect } from '../features/files/multiSelect.js';
import { favorites } from '../features/library/favorites.js';
import { musicView } from '../features/library/music.js';
import { photosView } from '../features/library/photos.js';
import { recent } from '../features/library/recent.js';
import { sharedView } from '../views/shared/sharedView.js';
import { loadFiles } from './filesView.js';
import { setActionsBarMode } from './main.js';
import { app, appElements } from './state.js';
import { loadTrashItems } from './trashView.js';
import { ui } from './ui.js';

/**
 * Sync the hidden class and inline display for the grid/list containers
 * based on the current view preference.
 */
function syncViewContainers() {
    const filesList = document.getElementById('files-list');
    const gridViewBtn = document.getElementById('grid-view-btn');
    const listViewBtn = document.getElementById('list-view-btn');

    const isGrid = app.currentView === 'grid';
    if (isGrid) {
        filesList?.classList.remove('files-list-view');
        filesList?.classList.add('files-grid-view');

        gridViewBtn?.classList.add('active');
        listViewBtn?.classList.remove('active');
    } else {
        filesList?.classList.add('files-list-view');
        filesList?.classList.remove('files-grid-view');

        gridViewBtn?.classList.remove('active');
        listViewBtn?.classList.add('active');
    }
}

/**
 * Hide file containers (used when switching to non-file views).
 * @param {boolean} show false to hide, true to show
 */
function toggleFileContainer(show) {
    const filesList = document.getElementById('files-list');
    filesList?.classList.toggle('hidden', !show);
}

/**
 * Mobile sidebar toggle functionality
 */
function initSidebarToggle() {
    const sidebarToggle = document.getElementById('sidebar-toggle');
    const sidebar = document.getElementById('sidebar');
    const sidebarOverlay = document.getElementById('sidebar-overlay');

    if (!sidebarToggle || !sidebar || !sidebarOverlay) return;

    function openSidebar() {
        sidebar?.classList.add('open');
        sidebarOverlay?.classList.add('active');
        document.body.style.overflow = 'hidden';
    }

    function closeSidebar() {
        sidebar?.classList.remove('open');
        sidebarOverlay?.classList.remove('active');
        document.body.style.overflow = '';
    }

    function toggleSidebar() {
        if (sidebar?.classList.contains('open')) {
            closeSidebar();
        } else {
            openSidebar();
        }
    }

    // Toggle button click
    sidebarToggle.addEventListener('click', toggleSidebar);

    // Close sidebar when clicking overlay
    sidebarOverlay.addEventListener('click', closeSidebar);

    // Close sidebar on escape key
    document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape' && sidebar.classList.contains('open')) {
            closeSidebar();
        }
    });

    // Close sidebar when navigating (nav item click on mobile)
    const navItems = sidebar.querySelectorAll('.nav-item');
    navItems.forEach((item) => {
        item.addEventListener('click', () => {
            if (window.innerWidth <= 768) {
                closeSidebar();
            }
        });
    });
}

// Initialize sidebar toggle when DOM is ready
document.addEventListener('DOMContentLoaded', initSidebarToggle);

/**
 * Derive section name from nav item's data-i18n attribute.
 * @param {HTMLElement} navItem - The nav item element
 * @returns {string|null} - Section name or null if not found
 */
function getSectionFromNavItem(navItem) {
    const i18nKey = navItem.querySelector('span[data-i18n]')?.getAttribute('data-i18n');
    return i18nKey ? i18nKey.replace('nav.', '') : null;
}

// Mapping section name to associated switch functions
/** @type {Record<String, Function>} */
export const SECTIONS_MAPPER = {
    files: switchToFilesSection,
    shared: switchToSharedSection,
    recent: switchToRecentFilesSection,
    favorites: switchToFavoritesSection,
    trash: switchToTrashSection,
    photos: switchToPhotosSection,
    music: switchToMusicSection
};

/**
 * Set the current active section, updating all view flags and nav UI.
 * @param {string} section - The section to activate ('files', 'shared', 'recent', 'favorites', 'trash')
 * @returns {boolean} true if the section changed
 */
function setCurrentSection(section) {
    if (app.currentSection === section) return false;

    // Set all view flags - true for active section, false for others
    Object.entries(SECTIONS_MAPPER).forEach(([key, flag]) => {
        app[flag] = key === section;
    });

    app.currentSection = section;

    // Update nav item active classes by finding matching item from DOM
    appElements.navItems?.forEach((item) => {
        const itemSection = getSectionFromNavItem(item);
        item.classList.toggle('active', itemSection === section);
    });

    // Update page title
    const titleKey = `nav.${section}`;
    // TODO check why no more used: const defaultTitle = section.charAt(0).toUpperCase() + section.slice(1);
    if (appElements.pageTitle) {
        appElements.pageTitle.textContent = i18n.t(titleKey);
        appElements.pageTitle.setAttribute('data-i18n', titleKey);
    }

    // Hide sharedView when switching to any other section
    if (section !== 'shared' && sharedView) {
        sharedView.hide();
    }

    // Hide photosView when switching to any other section
    if (section !== 'photos' && photosView) {
        photosView.hide();
    }

    // Hide musicView when switching to any other section
    if (section !== 'music' && musicView) {
        musicView.hide();
    }

    return true;
}

function switchToSharedSection() {
    if (!setCurrentSection('shared')) return;

    // Hide breadcrumb (only shown in Files view)
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.add('hidden');

    // Hide actions-bar for shared view
    setActionsBarMode('hidden');

    //reset files view + remove any error
    ui.resetFilesList();

    // Hide file containers
    toggleFileContainer(false);

    // Show shared view
    sharedView.init().then(() => {
        sharedView.show();
    });

    if (multiSelect) multiSelect.clear();
}

function switchToFilesSection() {
    if (!setCurrentSection('files')) return;

    // Set actions bar mode
    setActionsBarMode('files', true);

    // Show breadcrumb (only in Files view)
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.remove('hidden');

    // show files container
    toggleFileContainer(true);

    // ensure correct view
    syncViewContainers();

    //reset files view + remove any error
    ui.resetFilesList();

    // Reset to home folder and update breadcrumb
    app.currentPath = app.userHomeFolderId || '';
    app.breadcrumbPath = [];
    ui.updateBreadcrumb();
    if (multiSelect) multiSelect.clear();

    // temp solution
    sharedView.loadItems().then(() => {
        loadFiles();
    });
}

function switchToFavoritesSection() {
    if (!setCurrentSection('favorites')) return;

    // Set actions bar mode
    setActionsBarMode('favorites');

    // Hide breadcrumb (only shown in Files view)
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.add('hidden');

    // show files container
    toggleFileContainer(true);

    // ensure correct view
    syncViewContainers();

    //reset files view + remove any error
    ui.resetFilesList();

    if (favorites) {
        // temp solution
        sharedView.loadItems().then(() => {
            favorites.displayFavorites();
        });
    } else {
        console.error('Favorites module not loaded or initialized');
        ui.showError(`
                <i class="fas fa-exclamation-circle empty-state-icon error"></i>
                <p>Error loading the favorites module</p>
            `);
    }

    if (multiSelect) multiSelect.clear();
}

function switchToRecentFilesSection() {
    if (!setCurrentSection('recent')) return;

    // Set actions bar mode
    setActionsBarMode('recent');

    // Hide breadcrumb (only shown in Files view)
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.add('hidden');

    // show files container
    toggleFileContainer(true);

    // ensure correct view
    syncViewContainers();

    //reset files view + remove any error
    ui.resetFilesList();

    if (recent) {
        sharedView.loadItems().then(() => {
            recent.displayRecentFiles();
        });
    } else {
        console.error('Recent files module not loaded or initialized');
        ui.showError(`
                <i class="fas fa-exclamation-circle empty-state-icon error"></i>
                <p>Error loading the recent module</p>
            `);
    }
    if (multiSelect) multiSelect.clear();
}

function switchToPhotosSection() {
    if (!setCurrentSection('photos')) return;

    // Hide breadcrumb
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.add('hidden');

    // Hide actions-bar (photos has its own upload via selection bar)
    setActionsBarMode('hidden');

    //reset files view + remove any error
    ui.resetFilesList();

    // Hide file containers
    toggleFileContainer(false);

    // Show photos view
    if (photosView) {
        photosView.show();
    }
    if (multiSelect) multiSelect.clear();
}

function switchToTrashSection() {
    setCurrentSection('trash');

    // Hide breadcrumb (only shown in Files view)
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.add('hidden');

    // Show files containers (to be filled with trash)
    // Hide file containers
    toggleFileContainer(true);

    setActionsBarMode('trash');

    //reset files view + remove any error
    ui.resetFilesList();

    //ensure buttons match the current view
    syncViewContainers();

    // Load trash items
    loadTrashItems();

    if (multiSelect) multiSelect.clear();
}

function switchToMusicSection() {
    if (!setCurrentSection('music')) return;

    // Hide breadcrumb
    const breadcrumb = document.querySelector('.breadcrumb');
    breadcrumb?.classList.add('hidden');

    // Hide file containers
    toggleFileContainer(false);

    // Hide actions-bar
    setActionsBarMode('hidden');

    // Reset files view + remove any error
    ui.resetFilesList();

    // Hide list header (created by resetFilesList)
    const listHeader = document.querySelector('.list-header');
    listHeader?.classList.add('hidden');

    // Show music view
    if (musicView) {
        musicView.show();
    }
    if (multiSelect) multiSelect.clear();
}

export {
    switchToFavoritesSection,
    switchToFilesSection,
    switchToMusicSection,
    switchToPhotosSection,
    switchToRecentFilesSection,
    switchToSharedSection,
    switchToTrashSection,
    syncViewContainers
};
