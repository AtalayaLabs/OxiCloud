import { i18n } from './i18n.js';

/**
 * OxiCloud – Notification Bell Module
 *
 * Centralised notification system that renders items inside the bell dropdown
 * in the top-bar. Upload progress, quota errors, and general messages all
 * go through this module.
 *
 * Public API (exported as `notifications`):
 *   addUploadBatch(totalFiles)       → batchId
 *   updateFile(batchId, fileName, pct, status)
 *   finishBatch(batchId, successCount, totalFiles)
 *   addNotification({ icon, iconClass, title, text })
 *   clear()
 */

const notifications = (() => {
    /* ── state ──────────────────────────────────────────────── */
    let _badgeCount = 0;
    let _batchSeq = 0;
    const _batches = {}; // batchId → { el, files:{}, totalFiles }

    /* ── DOM refs (resolved lazily) ─────────────────────────── */
    const $ = (id) => document.getElementById(id);

    /* ── bell toggle ────────────────────────────────────────── */
    function _initBell() {
        const bellBtn = $('notif-bell-btn');
        const wrapper = $('notif-wrapper');
        const clearBtn = $('notif-clear-btn');

        if (!bellBtn) return;

        bellBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            const open = wrapper?.classList.toggle('open');
            bellBtn.classList.toggle('active', open);

            // Close user-menu if it's open
            const um = $('user-menu-wrapper');
            if (um) um.classList.remove('open');

            if (open) _clearBadge();
        });

        // Close on outside click
        document.addEventListener('click', (e) => {
            if (!wrapper?.contains(/** @type {Node | null} */ (e.target))) {
                close();
            }
        });

        // Clear all
        if (clearBtn) {
            clearBtn.addEventListener('click', (e) => {
                e.stopPropagation();
                clear();
            });
        }
    }

    function close() {
        const bellBtn = $('notif-bell-btn');
        const wrapper = $('notif-wrapper');
        wrapper?.classList.remove('open');
        bellBtn?.classList.remove('active');
    }

    /* ── badge helpers ──────────────────────────────────────── */
    function _incrementBadge() {
        _badgeCount++;
        _renderBadge();
        _ringBell();
    }
    function _clearBadge() {
        _badgeCount = 0;
        _renderBadge();
    }
    function _renderBadge() {
        const badge = $('notif-badge');
        if (!badge) return;
        if (_badgeCount > 0) {
            badge.classList.remove('hidden');
            badge.textContent = _badgeCount > 99 ? '99+' : String(_badgeCount);
        } else {
            badge.classList.add('hidden');
        }
    }
    function _ringBell() {
        const btn = $('notif-bell-btn');
        if (!btn) return;
        btn.classList.remove('ring');
        // Force reflow so the animation restarts
        void btn.offsetWidth;
        btn.classList.add('ring');
    }

    /* ── empty state ────────────────────────────────────────── */
    function _showEmptyIfNeeded() {
        const body = $('notif-panel-body');
        const empty = $('notif-empty');
        if (!body || !empty) return;
        // Any real items?
        const hasItems = body.querySelector('.notif-item') !== null;
        empty.style.display = hasItems ? 'none' : '';
    }

    /* ── generic notification ───────────────────────────────── */
    function addNotification({ icon = 'fa-info-circle', iconClass = 'upload', title = '', text = '' }) {
        const body = $('notif-panel-body');
        if (!body) return;

        const item = document.createElement('div');
        item.className = 'notif-item';
        item.innerHTML = `
            <div class="notif-item-icon ${iconClass}"><i class="fas ${icon}"></i></div>
            <div class="notif-item-body">
                <div class="notif-item-title">${_esc(title)}</div>
                <div class="notif-item-text" title="${_esc(text)}">${_esc(text)}</div>
                <div class="notif-item-time">${_timeAgo()}</div>
            </div>
        `;
        // Insert at top
        body.insertBefore(item, body.firstChild);

        // If panel is closed, bump badge
        const wrapper = $('notif-wrapper');
        if (!wrapper?.classList.contains('open')) {
            _incrementBadge();
        }
        _showEmptyIfNeeded();
    }

    /* ── upload batch API ───────────────────────────────────── */

    /**
     * Start tracking a new upload batch.  Returns a batchId string.
     * Always uses compact folder-level display: one progress bar + counter.
     * @param {number} totalFiles
     * @param {string} [folderName]  root folder name (for folder uploads)
     */
    function addUploadBatch(totalFiles, folderName) {
        const batchId = `batch-${++_batchSeq}`;
        const body = $('notif-panel-body');
        if (!body) return batchId;

        const item = document.createElement('div');
        item.className = 'notif-item';
        item.id = batchId;

        const uploadingText = folderName ? `📁 ${i18n.t('upload.uploading')} ${_esc(folderName)}…` : i18n.t('upload.uploading');
        const filesLabel = i18n.t('upload.files');

        item.innerHTML = `
            <div class="notif-item-icon upload"><i class="fas fa-cloud-upload-alt"></i></div>
            <div class="notif-item-body">
                <div class="notif-item-title">${uploadingText}</div>
                <div class="notif-upload-current" id="${batchId}-current"></div>
                <div class="notif-upload-progress">
                    <div class="notif-upload-bar"><div class="notif-upload-fill" id="${batchId}-fill"></div></div>
                    <div class="notif-upload-detail">
                        <span class="notif-upload-pct" id="${batchId}-pct">0%</span>
                        <span class="notif-upload-stats" id="${batchId}-stats">0 / ${totalFiles} ${filesLabel}</span>
                    </div>
                </div>
                <div class="notif-item-time">${_timeAgo()}</div>
            </div>
        `;

        // Insert at top
        body.insertBefore(item, body.firstChild);

        _batches[batchId] = {
            el: item,
            totalFiles,
            completed: 0,
            successCount: 0,
            errorCount: 0,
            lastLabelUpdateTs: 0,
            lastLabelFile: ''
        };
        _showEmptyIfNeeded();

        // Auto open
        const wrapper = $('notif-wrapper');
        const bellBtn = $('notif-bell-btn');
        if (wrapper && !wrapper.classList.contains('open')) {
            wrapper.classList.add('open');
            if (bellBtn) bellBtn.classList.add('active');
        }

        return batchId;
    }

    /**
     * Update the current-file label inside a batch.
     * This does NOT create any DOM rows — just a single text update.
     * @param {string} batchId
     * @param {string} fileName
     * @param {number} pct       0-100
     * @param {'uploading'|'done'|'error'} status
     */
    function updateFile(batchId, fileName, pct, status) {
        const batch = _batches[batchId];
        if (!batch) return;

        if (status === 'error') batch.errorCount = (batch.errorCount || 0) + 1;

        // Update the current-file label AND progress bar during upload
        if (status === 'uploading') {
            const now = Date.now();
            const fileChanged = batch.lastLabelFile !== fileName;
            // Throttle DOM updates to avoid reflow storms (every 300ms or on file change)
            const shouldUpdate = fileChanged || now - (batch.lastLabelUpdateTs || 0) >= 300 || pct >= 100;
            if (!shouldUpdate) return;

            // Show just the file name being uploaded (truncate long paths)
            const curEl = $(`${batchId}-current`);
            if (curEl) {
                const shortName = fileName.length > 50 ? `…${fileName.slice(-49)}` : fileName;
                curEl.textContent = shortName;
            }
            batch.lastLabelFile = fileName;
            batch.lastLabelUpdateTs = now;

            // Update progress bar with per-file granularity:
            // overall% = (completed_files + current_file_fraction) / total_files
            const overallPct = Math.round(((batch.completed + pct / 100) / batch.totalFiles) * 100);
            const fillEl = $(`${batchId}-fill`);
            const pctEl = $(`${batchId}-pct`);
            if (fillEl) fillEl.style.width = `${overallPct}%`;
            if (pctEl) pctEl.textContent = `${overallPct}%`;
        }
    }

    /**
     * Mark a file as completed within a batch (updates overall bar).
     * DOM updates are throttled to every 5 files to avoid reflow starvation.
     */
    function fileCompleted(batchId, success) {
        const batch = _batches[batchId];
        if (!batch) return;
        batch.completed++;
        if (success) batch.successCount++;

        // Throttle DOM updates: every 5 files, or on the very last file
        const isLast = batch.completed >= batch.totalFiles;
        if (!isLast && batch.completed % 5 !== 0) return;

        const pctVal = Math.round((batch.completed / batch.totalFiles) * 100);
        const fillEl = $(`${batchId}-fill`);
        const pctEl = $(`${batchId}-pct`);
        const statsEl = $(`${batchId}-stats`);

        const filesLabel = i18n.t('upload.files');

        if (fillEl) fillEl.style.width = `${pctVal}%`;
        if (pctEl) pctEl.textContent = `${pctVal}%`;
        if (statsEl) statsEl.textContent = `${batch.completed} / ${batch.totalFiles} ${filesLabel}`;
    }

    /**
     * Finalise a batch – update icon and title.
     */
    function finishBatch(batchId, successCount, totalFiles) {
        const batch = _batches[batchId];
        if (!batch) return;

        const fillEl = $(`${batchId}-fill`);
        if (fillEl) {
            fillEl.style.width = '100%';
            fillEl.classList.add(successCount === totalFiles ? 'done' : 'error');
        }

        const titleEl = batch.el.querySelector('.notif-item-title');
        const iconEl = batch.el.querySelector('.notif-item-icon');

        // Clear the current-file label
        const curEl = $(`${batchId}-current`);
        if (curEl) curEl.textContent = '';

        const completeText = i18n.t('upload.complete', {
            count: successCount,
            total: totalFiles
        });
        if (titleEl) titleEl.textContent = completeText;

        if (iconEl) {
            if (successCount === totalFiles) {
                iconEl.className = 'notif-item-icon success';
                iconEl.innerHTML = '<i class="fas fa-check-circle"></i>';
            } else {
                iconEl.className = 'notif-item-icon error';
                iconEl.innerHTML = '<i class="fas fa-exclamation-triangle"></i>';
            }
        }

        // If the panel is closed, bump badge
        const wrapper = $('notif-wrapper');
        if (!wrapper?.classList.contains('open')) {
            _incrementBadge();
        }
    }

    /* ── clear all ──────────────────────────────────────────── */
    function clear() {
        const body = $('notif-panel-body');
        if (!body) return;
        // Remove all notif-items
        body.querySelectorAll('.notif-item').forEach((el) => {
            el.remove();
        });
        _clearBadge();
        _showEmptyIfNeeded();
        // automatically close notification center on clear
        setTimeout(() => {
            close();
        }, 600);
    }

    /* ── util ───────────────────────────────────────────────── */
    function _esc(s) {
        const d = document.createElement('div');
        d.textContent = s;
        return d.innerHTML;
    }
    function _timeAgo() {
        const now = new Date();
        const h = String(now.getHours()).padStart(2, '0');
        const m = String(now.getMinutes()).padStart(2, '0');
        return `${h}:${m}`;
    }

    /* ── init on DOM ready ──────────────────────────────────── */
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', _initBell);
    } else {
        _initBell();
    }

    /* ── public API ─────────────────────────────────────────── */
    return {
        addUploadBatch,
        updateFile,
        fileCompleted,
        finishBatch,
        addNotification,
        clear
    };
})();

export { notifications };
