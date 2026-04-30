/**
 * OxiCloud - UI notifications adapter
 * Isolates notification rendering policy from ui.js.
 */

import { notifications } from '../core/notifications.js';

const uiNotifications = {
    /**
     *
     * @param {string} title
     * @param {string} message
     * @returns
     */
    show(title, message) {
        const normalizedTitle = String(title || '').toLowerCase();
        let icon = 'fa-info-circle';
        let iconClass = 'upload';

        if (normalizedTitle.includes('error') || normalizedTitle.includes('failed') || normalizedTitle.includes('fail')) {
            icon = 'fa-exclamation-circle';
            iconClass = 'error';
        } else if (normalizedTitle.includes('favorite') || normalizedTitle.includes('favorit') || normalizedTitle.includes('fav')) {
            icon = 'fa-star';
            iconClass = 'success';
        } else if (
            normalizedTitle.includes('delete') ||
            normalizedTitle.includes('removed') ||
            normalizedTitle.includes('trash') ||
            normalizedTitle.includes('rename') ||
            normalizedTitle.includes('complete')
        ) {
            icon = 'fa-check-circle';
            iconClass = 'success';
        }

        notifications.addNotification({
            icon,
            iconClass,
            title: title || '',
            text: message || ''
        });
        return;
    }
};

export { uiNotifications };
