/**
 * Search view orchestration logic
 */

import { search } from '../features/files/search.js';
import { resolveHomeFolder } from './authSession.js';
import { app } from './state.js';
import { ui } from './ui.js';

/**
 * @param {string} query
 * @param {string} [sortBy]
 */
async function performSearch(query, sortBy) {
    console.log(`Performing search for: "${query}" (sort: ${sortBy || 'relevance'})`);

    try {
        app.isSearchMode = true;
        ui.updateBreadcrumb();

        ui.showError(`<h3><i class="fas fa-spinner fa-spin search-spinner"></i> Searching for "${query}"...</h3>`);

        const options = {
            recursive: true,
            limit: 100,
            sort_by: sortBy || 'relevance'
        };

        if (app.currentSection !== 'trash') {
            // Ensure we have a valid folder_id before searching
            if (!app.currentPath || app.currentPath === '') {
                await resolveHomeFolder();
            }

            // Only set folder_id if we have a valid value
            if (app.currentPath && app.currentPath !== '') {
                options.folder_id = app.currentPath;
            }
            // If still no valid folder_id, search will be global (without folder_id)
        }

        const searchResults = await search.searchFiles(query, options);
        search.displaySearchResults(searchResults);
    } catch (error) {
        console.error('Search error:', error);
        ui.showNotification('Error', 'Error performing search');
    }
}

document.addEventListener('search-resort', (e) => {
    const event = /** @type {CustomEvent<{sort_by: string}>} */ (e);
    const searchInput = /** @type {HTMLInputElement} */ (document.querySelector('.search-container input'));
    if (searchInput?.value.trim()) {
        performSearch(searchInput.value.trim(), event.detail.sort_by);
    }
});

export { performSearch };
