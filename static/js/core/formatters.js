/**
 * OxiCloud - Shared format and escaping utilities
 * Centralized global helpers for date/size/text formatting and XSS-safe escaping.
 */

/**
 *
 * @param {string} str
 * @returns {string}
 */
function escapeHtml(str) {
    if (typeof str !== 'string') return '';
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#039;');
}

/**
 *
 * @param {number} bytes
 * @returns {string}
 */
function formatFileSize(bytes) {
    if (bytes === 0) return '0 Bytes';

    const k = 1024;
    const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));

    return `${parseFloat((bytes / k ** i).toFixed(2))} ${sizes[i]}`;
}

/// Formats a byte count for quota display. When bytes is 0, returns "∞" (unlimited).
/**
 *
 * @param {number} bytes
 * @returns {string}
 */
function formatQuotaSize(bytes) {
    if (bytes === 0) return '∞';
    return formatFileSize(bytes);
}

/**
 *
 * @param {Date | number| null} value
 * @returns {string}
 */
function formatDateTime(value) {
    if (!value) return '';
    let dateValue;
    if (value instanceof Date) {
        dateValue = value;
    } else if (typeof value === 'number') {
        dateValue = new Date(value < 1e12 ? value * 1000 : value);
    } else {
        dateValue = new Date(value);
    }
    if (Number.isNaN(dateValue.getTime())) return String(value);
    return `${dateValue.toLocaleDateString()} ${dateValue.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`;
}

/**
 *
 * @param {Date | number| null} value
 * @returns {string}
 */
function formatDateShort(value) {
    if (!value) return 'N/A';
    const dateValue = typeof value === 'number' ? new Date(value * 1000) : new Date(value);
    if (Number.isNaN(dateValue.getTime())) return String(value);
    return dateValue.toLocaleDateString(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric'
    });
}

const TEXT_TYPES = [
    'application/json',
    'application/xml',
    'application/javascript',
    'application/x-sh',
    'application/x-yaml',
    'application/toml',
    'application/x-toml',
    'application/sql'
];
// FIXME: move is to another file
/**
 *
 * @param {string} mimeType
 * @returns {boolean}
 */
function isTextViewable(mimeType) {
    if (!mimeType) return false;
    if (mimeType.startsWith('text/')) return true;

    return TEXT_TYPES.includes(mimeType);
}

export { escapeHtml, formatDateShort, formatDateTime, formatFileSize, formatQuotaSize, isTextViewable };
