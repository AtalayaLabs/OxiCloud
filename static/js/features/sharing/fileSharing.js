/**
 * OxiCloud - File Sharing Module
 * All operations go through the backend API at /api/shares.
 * No localStorage is used for share data.
 */

import { switchToSharedSection } from '../../app/navigation.js';
import { ui } from '../../app/ui.js';
import { getCsrfHeaders } from '../../core/csrf.js';
import { formatDateTime } from '../../core/formatters.js';

const fileSharing = {
    /** Auth header helper — tokens are in HttpOnly cookies now */
    _headers(json = true) {
        const h = { ...getCsrfHeaders() };
        if (json) h['Content-Type'] = 'application/json';
        return h;
    },

    /**
     * Create a shared link via backend API
     * @param {string} itemId - ID of the file or folder
     * @param {string} itemType - 'file' or 'folder'
     * @param {Object} options - { name, password, expirationDate, permissions }
     * @returns {Promise<Object>} ShareDto from backend
     */
    async createSharedLink(itemId, itemType, options = {}) {
        const body = {
            item_id: itemId,
            item_name: options.name || null,
            item_type: itemType,
            password: options.password || null,
            expires_at: options.expirationDate ? Math.floor(new Date(options.expirationDate).getTime() / 1000) : null,
            permissions: options.permissions || {
                read: true,
                write: false,
                reshare: false
            }
        };

        const res = await fetch('/api/shares', {
            method: 'POST',
            headers: this._headers(),
            body: JSON.stringify(body)
        });

        if (!res.ok) {
            const err = await res.json().catch(() => ({}));
            throw new Error(err.error || `Server error ${res.status}`);
        }

        return await res.json();
    },

    /**
     * Get all shared links for the current user
     * @returns {Promise<Array>} Array of ShareDto
     */
    async getSharedLinks() {
        try {
            const res = await fetch('/api/shares?page=1&per_page=1000', {
                headers: this._headers(false)
            });
            if (!res.ok) return [];
            const data = await res.json();
            return data.items || [];
        } catch (error) {
            console.error('Error fetching shared links:', error);
            return [];
        }
    },

    /**
     * Get shared links for a specific item (server-side filtered)
     * @param {string} itemId
     * @param {string} itemType - 'file' or 'folder'
     * @returns {Promise<Array>} Shares for this item
     */
    async getSharedLinksForItem(itemId, itemType) {
        try {
            const params = new URLSearchParams({
                item_id: itemId,
                item_type: itemType
            });
            const res = await fetch(`/api/shares?${params}`, {
                headers: this._headers(false)
            });
            if (!res.ok) return [];
            const data = await res.json();
            return Array.isArray(data) ? data : data.items || [];
        } catch (error) {
            console.error('Error getting shared links for item:', error);
            return [];
        }
    },

    /**
     * Check if an item has any shared links
     * @returns {Promise<boolean>}
     */
    async hasSharedLinks(itemId, itemType) {
        const links = await this.getSharedLinksForItem(itemId, itemType);
        return links.length > 0;
    },

    /**
     * Update a shared link
     * @param {string} shareId
     * @param {Object} updateData - { permissions, password, expires_at }
     * @returns {Promise<Object>} Updated ShareDto
     */
    async updateSharedLink(shareId, updateData) {
        const body = {};
        if (updateData.permissions) body.permissions = updateData.permissions;
        if (updateData.password !== undefined) body.password = updateData.password;
        if (updateData.expires_at !== undefined) body.expires_at = updateData.expires_at;

        const res = await fetch(`/api/shares/${shareId}`, {
            method: 'PUT',
            headers: this._headers(),
            body: JSON.stringify(body)
        });

        if (!res.ok) {
            const err = await res.json().catch(() => ({}));
            throw new Error(err.error || `Server error ${res.status}`);
        }

        return await res.json();
    },

    /**
     * Delete a shared link
     * @param {string} shareId
     * @returns {Promise<boolean>}
     */
    async removeSharedLink(shareId) {
        try {
            const res = await fetch(`/api/shares/${shareId}`, {
                method: 'DELETE',
                headers: this._headers(false)
            });
            return res.ok || res.status === 204;
        } catch (error) {
            console.error('Error removing shared link:', error);
            return false;
        }
    },

    /**
     * Copy a shared link to clipboard
     * @param {string} url
     */
    async copyLinkToClipboard(url) {
        try {
            await navigator.clipboard.writeText(url);
            ui.showNotification('Link copied', 'Link copied to clipboard');
            return true;
        } catch (error) {
            console.error('Error copying to clipboard:', error);
            ui.showNotification('Error', 'Could not copy link');
            return false;
        }
    },

    /**
     * Format expiration date for display (Unix timestamp in seconds or ISO string)
     * @param {number|Date} value
     * @returns {string}
     */
    formatExpirationDate(value) {
        if (!value) return 'No expiration';
        return formatDateTime(value);
    },

    /**
     * Send a notification about a shared resource (stub — no backend endpoint yet)
     * @param {string} shareUrl
     * @param {string} recipientEmail
     * @param {string} _message
     * @returns {Promise<boolean>}
     */
    async sendShareNotification(shareUrl, recipientEmail, _message = '') {
        // TODO: implement backend endpoint for email notifications
        console.log(`Share notification for ${shareUrl} sent to ${recipientEmail}`);
        ui.showNotification('Notification sent', `Notification sent to ${recipientEmail}`);
        return true;
    },

    /**
     * Initialize file sharing event listeners
     */
    init() {
        console.log('File sharing module initialized (API-backed)');
        document.querySelectorAll('.nav-item').forEach((item) => {
            const span = item.querySelector('span');
            if (span && span.getAttribute('data-i18n') === 'nav.shared') {
                item.addEventListener('click', () => {
                    switchToSharedSection();
                });
            }
        });
    }
};

export { fileSharing };
