import { i18n } from './i18n.js';
import { replaceIconsInElement } from './icons.js';

/**
 * Modal System for OxiCloud
 * Provides modern, styled modals to replace browser prompts/alerts
 */

const Modal = {
    // Modal element references
    /** @private @type {HTMLElement | null} */
    overlay: null,
    // FIXME: unused ?
    container: null,
    /** @private @type {HTMLElement | null} */
    icon: null,
    /** @private @type {HTMLElement | null} */
    title: null,
    /** @private @type {HTMLElement | null} */
    label: null,
    /** @private @type {HTMLElement | null} */
    input: null,
    /** @private @type {HTMLElement | null} */
    cancelBtn: null,
    /** @private @type {HTMLElement | null} */
    confirmBtn: null,
    /** @private @type {HTMLElement | null} */
    closeBtn: null,

    // Current callback
    onConfirm: null,
    onCancel: null,

    // Rename mode: select only name without extension
    _selectNameOnly: false,

    /**
     * Initialize modal system
     */
    init() {
        this.overlay = document.getElementById('input-modal');
        if (!this.overlay) {
            console.warn('Modal overlay not found');
            return;
        }

        this.icon = document.getElementById('modal-icon');
        this.title = document.getElementById('modal-title');
        this.label = document.getElementById('modal-label');
        this.input = document.getElementById('modal-input');
        this.cancelBtn = document.getElementById('modal-cancel-btn');
        this.confirmBtn = document.getElementById('modal-confirm-btn');
        this.closeBtn = document.getElementById('modal-close-btn');

        // Event listeners
        this.cancelBtn?.addEventListener('click', () => this.close(false));
        this.closeBtn?.addEventListener('click', () => this.close(false));
        this.confirmBtn?.addEventListener('click', () => this.confirm());

        // Close on overlay click
        this.overlay.addEventListener('click', (e) => {
            if (e.target === this.overlay) {
                this.close(false);
            }
        });

        // Handle Enter and Escape keys
        this.input?.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') {
                e.preventDefault();
                this.confirm();
            } else if (e.key === 'Escape') {
                this.close(false);
            }
        });
    },

    /**
     * Show input modal (replacement for prompt())
     * @param {Object} options - Modal configuration
     * @param {string} options.title - Modal title
     * @param {string} options.label - Input label
     * @param {string} options.placeholder - Input placeholder
     * @param {string} options.value - Initial input value
     * @param {string} options.icon - Font Awesome icon class (e.g., 'fa-folder-plus')
     * @param {string} options.confirmText - Confirm button text
     * @param {string} options.cancelText - Cancel button text
     * @returns {Promise<string|null>} - Resolves with input value or null if cancelled
     */
    prompt(options = {}) {
        return new Promise((resolve) => {
            const { title = 'Input', label = '', placeholder = '', value = '', icon = 'fa-keyboard', confirmText = null, cancelText = null } = options;

            // Set modal content - update the icon
            const iconContainer = document.querySelector('.modal-icon');
            if (iconContainer) {
                // Replace contents with a fresh <i> that icons.js will convert
                iconContainer.innerHTML = `<i id="modal-icon" class="fas ${icon}"></i>`;
                this.icon = document.getElementById('modal-icon');
                // Let icons.js convert it to SVG
                if (replaceIconsInElement) {
                    replaceIconsInElement(iconContainer);
                    this.icon = document.getElementById('modal-icon');
                }
            }
            this.title.textContent = title;
            this.label.textContent = label;
            this.input.placeholder = placeholder;
            this.input.value = value;

            if (confirmText) {
                this.confirmBtn.textContent = confirmText;
            } else {
                this.confirmBtn.textContent = i18n.t('actions.confirm');
            }

            if (cancelText) {
                this.cancelBtn.textContent = cancelText;
            } else {
                this.cancelBtn.textContent = i18n.t('actions.cancel');
            }

            // Set callbacks
            this.onConfirm = () => {
                const inputValue = this.input.value.trim();
                resolve(inputValue || null);
            };
            this.onCancel = () => resolve(null);

            // Show modal
            this.open();
        });
    },

    /**
     * Show modal for creating new folder
     * @returns {Promise<string|null>}
     */
    promptNewFolder() {
        return this.prompt({
            title: i18n.t('dialogs.new_folder_title'),
            label: i18n.t('dialogs.folder_name'),
            placeholder: i18n.t('dialogs.folder_placeholder'),
            icon: 'fa-folder-plus',
            confirmText: i18n.t('actions.create')
        });
    },

    /**
     * Show modal for renaming
     * @param {string} currentName - Current name of file/folder
     * @param {boolean} isFolder - Whether it's a folder
     * @returns {Promise<string|null>}
     */
    promptRename(currentName, isFolder = false) {
        this._selectNameOnly = !isFolder;

        return this.prompt({
            title: i18n.t('dialogs.rename_title'),
            label: i18n.t('dialogs.new_name'),
            placeholder: '',
            value: currentName,
            icon: isFolder ? 'fa-folder' : 'fa-file',
            confirmText: i18n.t('actions.rename')
        });
    },

    /**
     * Open the modal
     */
    open() {
        if (!this.overlay) return;

        // Show overlay
        this.overlay.classList.remove('hidden');

        // Trigger animation
        requestAnimationFrame(() => {
            this.overlay.classList.add('active');
        });

        // Focus input after animation
        setTimeout(() => {
            this.input.focus();

            if (this._selectNameOnly) {
                // Select only the filename, excluding the extension
                const value = this.input.value;
                const lastDot = value.lastIndexOf('.');
                if (lastDot > 0) {
                    this.input.setSelectionRange(0, lastDot);
                } else {
                    this.input.select();
                }
                this._selectNameOnly = false;
            } else {
                this.input.select();
            }
        }, 100);
    },

    /**
     * Close the modal
     * @param {boolean} confirmed - Whether the action was confirmed
     */
    close(confirmed = false) {
        if (!this.overlay) return;

        this.overlay.classList.remove('active');

        setTimeout(() => {
            this.overlay.classList.add('hidden');

            if (!confirmed && this.onCancel) {
                this.onCancel();
            }

            // Clear callbacks
            this.onConfirm = null;
            this.onCancel = null;
        }, 200);
    },

    /**
     * Confirm the action
     */
    confirm() {
        if (this.onConfirm) {
            this.onConfirm();
        }
        this.close(true);
    }
};

// Initialize when DOM is ready
document.addEventListener('DOMContentLoaded', () => {
    Modal.init();
});

// Export for use in other modules
export { Modal };
