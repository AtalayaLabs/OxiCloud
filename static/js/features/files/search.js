/**
 * OxiCloud - Search Module (thin client)
 *
 * All search processing (filtering, scoring, sorting, categorization,
 * icon mapping, size formatting) is performed on the Rust backend.
 * This module is a pure rendering client — it sends requests and
 * displays the enriched results returned by the server.
 */

import { loadFiles } from '../../app/filesView.js';
import { app } from '../../app/state.js';
import { ui } from '../../app/ui.js';
import { getAuthHeaders } from './fileOperations.js';

const search = {
    /**
     * Perform a search using query parameters.
     * The backend handles all processing and returns enriched results with
     * relevance_score, icon_class, category, size_formatted, etc.
     *
     * @param {string} query - Search query
     * @param {Object} options - Additional search options
     * @returns {Promise<Object>} - Enriched search results from backend
     */
    async searchFiles(query, options = {}) {
        try {
            const params = new URLSearchParams();
            params.append('query', query);

            if (options.folder_id) params.append('folder_id', options.folder_id);
            if (options.recursive !== undefined) params.append('recursive', options.recursive);
            if (options.file_types) params.append('type', options.file_types);
            if (options.min_size) params.append('min_size', options.min_size);
            if (options.max_size) params.append('max_size', options.max_size);
            if (options.created_after) params.append('created_after', options.created_after);
            if (options.created_before) params.append('created_before', options.created_before);
            if (options.modified_after) params.append('modified_after', options.modified_after);
            if (options.modified_before) params.append('modified_before', options.modified_before);
            if (options.limit) params.append('limit', options.limit);
            if (options.offset) params.append('offset', options.offset);
            if (options.sort_by) params.append('sort_by', options.sort_by);

            const url = `/api/search?${params.toString()}`;
            console.log(`[search] GET ${url}`);

            const response = await fetch(url, { headers: getAuthHeaders() });

            if (response.ok) {
                return await response.json();
            } else {
                let errorText = '';
                try {
                    const errorJson = await response.json();
                    errorText = errorJson.error || response.statusText;
                } catch (_e) {
                    errorText = response.statusText;
                }
                console.error(`Search error: ${errorText}`);
                throw new Error(`Search failed: ${errorText}`);
            }
        } catch (error) {
            console.error('Error performing search:', error);
            ui.showNotification('Error', 'Error performing search');
            return {
                files: [],
                folders: [],
                total_count: 0,
                query_time_ms: 0,
                sort_by: 'relevance'
            };
        }
    },

    /**
     * Get autocomplete suggestions from the backend.
     * Returns lightweight name suggestions without full search overhead.
     *
     * @param {string} query - Prefix to search for
     * @param {Object} options - { folder_id, limit }
     * @returns {Promise<Object>} - { suggestions: [...], query_time_ms }
     */
    async getSuggestions(query, options = {}) {
        try {
            const params = new URLSearchParams();
            params.append('query', query);
            if (options.folder_id) params.append('folder_id', options.folder_id);
            if (options.limit) params.append('limit', options.limit);

            const url = `/api/search/suggest?${params.toString()}`;
            const response = await fetch(url, { headers: getAuthHeaders() });

            if (response.ok) {
                return await response.json();
            } else {
                return { suggestions: [], query_time_ms: 0 };
            }
        } catch (error) {
            console.error('Error getting suggestions:', error);
            return { suggestions: [], query_time_ms: 0 };
        }
    },

    /**
     * Display search results in the UI.
     *
     * Uses server-computed enriched data:
     * - icon_class: Font Awesome class for file type icon
     * - size_formatted: Human-readable file size (e.g. "2.5 MB")
     * - category: Content category (image, video, document, code, archive, audio, other)
     * - relevance_score: 0-100 match quality
     * - query_time_ms: Server-side query execution time
     * - sort_by: Active sort order
     *
     * @param {Object} results - Enriched search results from backend
     */
    displaySearchResults(results) {
        ui.resetFilesList(); // ensure also list visible & error hidden
        //FIXME: move into action rather bage sticky header + hide bread crumb ? + note also that search result does not consider section
        const pageStickyHeader = document.getElementById('page-sticky-header');

        // Search results header with query time and sort controls
        const totalCount = results.total_count || results.files.length + results.folders.length;
        const queryTimeText = results.query_time_ms !== undefined ? ` <span class="search-time">(${results.query_time_ms}ms)</span>` : '';

        const searchHeader = document.createElement('div');
        searchHeader.className = 'search-results-header';
        searchHeader.innerHTML = `
            <h3>Search results (${totalCount})${queryTimeText}</h3>
            <div class="search-controls">
                <select id="search-sort-select" class="search-sort-select" title="Sort by">
                    <option value="relevance"${results.sort_by === 'relevance' ? ' selected' : ''}>Relevance</option>
                    <option value="name"${results.sort_by === 'name' ? ' selected' : ''}>Name A-Z</option>
                    <option value="name_desc"${results.sort_by === 'name_desc' ? ' selected' : ''}>Name Z-A</option>
                    <option value="date_desc"${results.sort_by === 'date_desc' ? ' selected' : ''}>Newest first</option>
                    <option value="date"${results.sort_by === 'date' ? ' selected' : ''}>Oldest first</option>
                    <option value="size_desc"${results.sort_by === 'size_desc' ? ' selected' : ''}>Largest first</option>
                    <option value="size"${results.sort_by === 'size' ? ' selected' : ''}>Smallest first</option>
                </select>
                <button class="btn btn-secondary" id="clear-search-btn">
                    <i class="fas fa-times"></i> Clear search
                </button>
            </div>
        `;
        const previousSearchHeader = document.querySelector('.search-results-header');

        if (previousSearchHeader) {
            previousSearchHeader.replaceWith(searchHeader);
        } else {
            pageStickyHeader?.appendChild(searchHeader);
        }

        // Sort dropdown — re-searches with new sort order (server-side)
        const sortSelect = /** @type {HTMLSelectElement} */ (document.getElementById('search-sort-select'));
        if (sortSelect) {
            sortSelect.addEventListener('change', () => {
                const searchInput = /** @type {HTMLInputElement} */ (document.querySelector('.search-container input'));
                if (searchInput?.value.trim()) {
                    const event = new CustomEvent('search-resort', {
                        detail: { sort_by: sortSelect.value }
                    });
                    document.dispatchEvent(event);
                }
            });
        }

        // Clear search
        const clearSearchBtn = document.getElementById('clear-search-btn');
        if (clearSearchBtn) {
            clearSearchBtn.addEventListener('click', () => {
                const searchInput = /** @type {HTMLInputElement} */ (document.querySelector('.search-container input'));
                searchInput.value = '';
                app.currentPath = '';
                app.isSearchMode = false;
                document.querySelector('.search-results-header')?.remove();
                ui.updateBreadcrumb();
                loadFiles();
            });
        }

        // Empty state
        if (results.files.length === 0 && results.folders.length === 0) {
            ui.showError(`
                <i class="fas fa-search empty-state-icon"></i>
                <p class="search-empty-text">No results found for this search</p>
            `);
            return;
        }

        // Render folders (server-provided enriched data)
        results.folders.forEach((folder) => {
            ui.addFolderToView(folder);
        });

        // Render files (server-provided enriched data)
        results.files.forEach((file) => {
            ui.addFileToView(file);
        });
    },

    /**
     * Clear the search cache on the server
     * @returns {Promise<boolean>}
     */
    async clearSearchCache() {
        try {
            const response = await fetch('/api/search/cache', {
                method: 'DELETE',
                headers: getAuthHeaders()
            });

            if (response.ok) {
                ui.showNotification('Cache cleared', 'Search cache cleared successfully');
                return true;
            } else {
                ui.showNotification('Error', 'Error clearing search cache');
                return false;
            }
        } catch (error) {
            console.error('Error clearing search cache:', error);
            ui.showNotification('Error', 'Error clearing search cache');
            return false;
        }
    }
};

export { search };
