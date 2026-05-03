/**
 * OxiCloud WOPI Editor Integration
 *
 * Opens document files in Collabora Online / OnlyOffice via WOPI protocol.
 * Supports two modes: in-app modal (default) and new browser tab.
 */

import { loadFiles } from '../../app/filesView.js';
import { ui } from '../../app/ui.js';

class WopiEditor {
    constructor() {
        this.editorModal = null;
        this._escHandler = null;
        this._messageHandler = null;
        this._supportedExtensions = null;
    }

    /**
     * Check if a file can be opened in a WOPI editor by extension.
     * Fetches supported extensions from the server (cached after first call).
     */
    async canEdit(filename) {
        const ext = filename.split('.').pop().toLowerCase();
        const supported = await this._getSupportedExtensions();
        return supported.includes(ext);
    }

    /**
     * Open file in a modal overlay (default mode).
     */
    async openInModal(fileId, fileName, action) {
        action = action || 'edit';
        // Error is thrown, caller can handle a classic view as fallback
        var data = await this._getEditorUrlWithFallback(fileId, fileName, action);
        this._showModal(data, fileName);
    }

    /**
     * Open file in a new browser tab.
     */
    async openInTab(fileId, fileName, action) {
        action = action || 'edit';
        try {
            const data = await this._getEditorUrlWithFallback(fileId, fileName, action);
            const hostUrl = `/wopi/edit/${encodeURIComponent(fileId)}?access_token=${encodeURIComponent(data.access_token)}`;
            window.open(hostUrl, '_blank');
        } catch (error) {
            console.error('Failed to open WOPI editor in tab:', error);
            ui.showNotification('Could not open the document editor.', 'error');
        }
    }

    /**
     * Fetch editor URL and WOPI token from the backend.
     */
    async _getEditorUrl(fileId, action) {
        const response = await fetch(`/api/wopi/editor-url?file_id=${encodeURIComponent(fileId)}&action=${encodeURIComponent(action)}`, {
            credentials: 'same-origin'
        });
        if (!response.ok) {
            const text = await response.text();
            throw new Error(`Editor URL request failed: ${response.status} ${text}`);
        }
        return response.json();
    }

    /**
     * Some WOPI file types, such as PDFs, are view-only.
     * If an edit request returns 422, retry once in view mode.
     */
    async _getEditorUrlWithFallback(fileId, fileName, action) {
        try {
            return await this._getEditorUrl(fileId, action);
        } catch (error) {
            if (!this._shouldRetryInViewMode(fileName, action, error)) {
                throw error;
            }

            return this._getEditorUrl(fileId, 'view');
        }
    }

    _shouldRetryInViewMode(fileName, action, error) {
        if (action !== 'edit' || !error || !error.message) {
            return false;
        }

        var ext = fileName.split('.').pop().toLowerCase();
        return ext === 'pdf' && error.message.indexOf('422') !== -1;
    }

    /**
     * Show the editor in a full-screen modal with iframe.
     */
    _showModal(editorData, fileName) {
        this.closeEditor();

        const modal = document.createElement('div');
        modal.id = 'wopi-editor-modal';
        modal.style.cssText = 'position:fixed;top:0;left:0;width:100%;height:100%;z-index:10000;background:#fff;';

        const header = document.createElement('div');
        header.style.cssText =
            'height:40px;background:#333;color:#fff;display:flex;align-items:center;justify-content:space-between;padding:0 16px;font-family:sans-serif;font-size:14px;';

        const title = document.createElement('span');
        title.textContent = fileName;
        header.appendChild(title);

        const closeBtn = document.createElement('button');
        closeBtn.textContent = '\u2715';
        closeBtn.style.cssText = 'background:none;border:none;color:#fff;cursor:pointer;font-size:18px;padding:4px 8px;';
        closeBtn.onclick = this.closeEditor.bind(this);
        header.appendChild(closeBtn);

        const form = document.createElement('form');
        form.id = 'wopi_form';
        form.target = 'wopi_frame';
        form.action = editorData.editor_url;
        form.method = 'post';
        form.style.display = 'none';

        const tokenInput = document.createElement('input');
        tokenInput.name = 'access_token';
        tokenInput.value = editorData.access_token;
        tokenInput.type = 'hidden';
        form.appendChild(tokenInput);

        const ttlInput = document.createElement('input');
        ttlInput.name = 'access_token_ttl';
        ttlInput.value = editorData.access_token_ttl;
        ttlInput.type = 'hidden';
        form.appendChild(ttlInput);

        const frameHolder = document.createElement('div');
        frameHolder.style.cssText = 'position:absolute;top:40px;left:0;right:0;bottom:0;';

        // Loading spinner (removed once the editor signals ready)
        const spinner = document.createElement('div');
        spinner.id = 'wopi-loading-spinner';
        spinner.style.cssText =
            'position:absolute;top:0;left:0;right:0;bottom:0;display:flex;align-items:center;justify-content:center;background:#f5f5f5;z-index:1;';
        spinner.innerHTML = '<i class="fas fa-spinner fa-spin empty-state-icon spinner"></i>';
        frameHolder.appendChild(spinner);

        const iframe = document.createElement('iframe');
        iframe.name = 'wopi_frame';
        iframe.title = 'Document Editor';
        iframe.style.cssText = 'width:100%;height:100%;border:none;';
        iframe.setAttribute('allowfullscreen', 'true');
        // Fix 9: allow clipboard access for copy/paste inside the editor
        iframe.setAttribute('allow', 'clipboard-read; clipboard-write');
        iframe.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-forms allow-popups allow-top-navigation allow-popups-to-escape-sandbox');
        frameHolder.appendChild(iframe);

        modal.appendChild(header);
        modal.appendChild(form);
        modal.appendChild(frameHolder);
        document.body.appendChild(modal);

        // ESC key handler
        this._escHandler = function (e) {
            if (e.key === 'Escape') this.closeEditor();
        }.bind(this);
        document.addEventListener('keydown', this._escHandler);

        // Fix 7: Listen for postMessage from the editor iframe
        this._messageHandler = function (e) {
            var data;
            try {
                data = JSON.parse(e.data);
            } catch (_) {
                return; // Not a JSON message — ignore
            }
            var msgId = data.MessageId || data.messageId || '';
            if (msgId === 'UI_Close' || msgId === 'close') {
                this.closeEditor();
            } else if (msgId === 'App_LoadingStatus') {
                const status = data.Values?.Status;
                if (status === 'Document_Loaded' || status === 'Frame_Ready') {
                    const sp = document.getElementById('wopi-loading-spinner');
                    if (sp) sp.remove();
                }
            }
        }.bind(this);
        window.addEventListener('message', this._messageHandler);

        form.submit();
        this.editorModal = modal;
    }

    /**
     * Close the editor modal and refresh the file list.
     */
    closeEditor() {
        var modal = document.getElementById('wopi-editor-modal');
        if (modal) modal.remove();
        if (this._escHandler) {
            document.removeEventListener('keydown', this._escHandler);
            this._escHandler = null;
        }
        if (this._messageHandler) {
            window.removeEventListener('message', this._messageHandler);
            this._messageHandler = null;
        }
        this.editorModal = null;

        // Refresh file list to pick up any saves
        loadFiles();
    }

    /**
     * Fetch supported extensions from the server (cached).
     */
    async _getSupportedExtensions() {
        if (this._supportedExtensions !== null) {
            return this._supportedExtensions;
        }
        return this._fetchSupportedExtensions();
    }

    /**
     * Fetch supported extensions from /wopi/supported-extensions.
     * Falls back to a hardcoded list on failure.
     */
    async _fetchSupportedExtensions() {
        try {
            const response = await fetch('/wopi/supported-extensions');
            if (response.ok) {
                const exts = await response.json();
                if (Array.isArray(exts) && exts.length > 0) {
                    this._supportedExtensions = exts;
                    return exts;
                }
            }
        } catch (_) {
            // Ignore — fall through to hardcoded list
        }
        // Fallback hardcoded list
        this._supportedExtensions = ['docx', 'doc', 'odt', 'rtf', 'txt', 'xlsx', 'xls', 'ods', 'csv', 'pptx', 'ppt', 'odp'];
        return this._supportedExtensions;
    }
}

// Global instance
export const wopiEditor = new WopiEditor();

// Prefetch supported extensions so canEdit() is fast on first use
wopiEditor._fetchSupportedExtensions();
