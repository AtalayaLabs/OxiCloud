/**
 * OxiCloud - Photos Lightbox
 * Full-screen image/video viewer with prev/next navigation.
 *
 * Media is never buffered in page memory: videos stream straight from the
 * API (the element's `src` is same-origin, so auth cookies travel
 * automatically and the browser issues Range requests — playback starts
 * progressively and seeking works without downloading the whole file).
 * Photos open with the server-cached `large` thumbnail; the full-resolution
 * original streams in only on demand via the toolbar expand button.
 */

import { getCsrfHeaders } from '../../core/csrf.js';
import { favorites } from '../library/favorites.js';

/** @import {FileItem, FileMetadata} from '../../core/types.js' */
/** @typedef {typeof import('./photos.js').photosView} PhotosView */

export const photosLightbox = {
    /** @type {Array<FileItem>} Items array reference */
    items: [],
    /** @type {number} Current index */
    index: -1,
    /** @type {HTMLElement|null} */
    _overlay: null,
    /** @type {(ev: KeyboardEvent) => any|null} */
    _keyHandler: null,
    /** @type {PhotosView|null} Reference to photosView, set after both modules load */
    _photosView: null,
    /**
     * Monotonic token identifying the most recent {@link photosLightbox._show}
     * call. Image load/error callbacks fire asynchronously, so a rapid
     * prev/next must not let a superseded item commit its (stale) content
     * over the newer one.
     */
    _showGeneration: 0,

    /**
     * Register the photosView reference (called from photos.js to avoid circular imports).
     * @param {any} pv
     */
    setPhotosView(pv) {
        this._photosView = pv;
    },

    /** Auth headers */
    _headers() {
        return getCsrfHeaders();
    },

    /**
     * Streaming URL of the original file. Same-origin, so media elements
     * send the auth cookie automatically and the browser handles Range.
     * @param {FileItem} item
     * @returns {string}
     */
    _originalUrl(item) {
        return `/api/files/${item.id}?inline=true`;
    },

    /**
     * URL of the server-cached `large` thumbnail (immutable, browser-cached).
     * @param {FileItem} item
     * @returns {string}
     */
    _thumbUrl(item) {
        return `/api/files/${item.id}/thumbnail/large`;
    },

    /**
     * Open lightbox at given index
     * @param {FileItem[]} items
     * @param {number} index
     */
    open(items, index) {
        this.items = items;
        this.index = index;
        this._createOverlay();
        this._show();
        this._bindKeys();
    },

    /** Close lightbox */
    close() {
        if (this._overlay) {
            this._overlay.classList.remove('active');
            setTimeout(() => {
                if (this._overlay) {
                    this._overlay.remove();
                    this._overlay = null;
                }
            }, 200);
        }
        this._unbindKeys();
    },

    /** Navigate to previous */
    prev() {
        if (this.index > 0) {
            this.index--;
            this._show();
        }
    },

    /** Navigate to next */
    next() {
        if (this.index < this.items.length - 1) {
            this.index++;
            this._show();
        }
    },

    /** Create the overlay DOM structure */
    _createOverlay() {
        if (this._overlay) this._overlay.remove();

        const el = document.createElement('div');
        el.className = 'photos-lightbox';
        el.innerHTML = `
            <div class="lightbox-info">
                <div class="lightbox-filename"></div>
                <div class="lightbox-meta"></div>
            </div>
            <button class="lightbox-close"><i class="fas fa-times"></i></button>
            <button class="lightbox-nav lightbox-prev"><i class="fas fa-chevron-left"></i></button>
            <div class="lightbox-content"></div>
            <button class="lightbox-nav lightbox-next"><i class="fas fa-chevron-right"></i></button>
            <div class="lightbox-toolbar">
                <button class="lb-fullres hidden" title="Full resolution"><i class="fas fa-expand"></i></button>
                <button class="lb-download" title="Download"><i class="fas fa-download"></i></button>
                <button class="lb-favorite" title="Favorite"><i class="far fa-star"></i></button>
                <button class="lb-delete" title="Delete"><i class="fas fa-trash"></i></button>
            </div>
            <div class="lightbox-counter"></div>
        `;
        document.body.appendChild(el);
        this._overlay = el;

        // Event listeners
        /** @type {HTMLButtonElement} */ (el.querySelector('.lightbox-close')).onclick = () => this.close();
        /** @type {HTMLButtonElement} */ (el.querySelector('.lightbox-prev')).onclick = () => this.prev();
        /** @type {HTMLButtonElement} */ (el.querySelector('.lightbox-next')).onclick = () => this.next();

        // Click backdrop to close
        el.addEventListener('click', (e) => {
            if (e.target === el || /** @type {HTMLElement} */ (e.target).classList.contains('lightbox-content')) {
                this.close();
            }
        });

        // Toolbar actions (`.lb-fullres` is wired per-item in `_show`)
        /** @type {HTMLButtonElement} */ (el.querySelector('.lb-download')).onclick = () => this._download();
        /** @type {HTMLButtonElement} */ (el.querySelector('.lb-favorite')).onclick = () => this._toggleFavorite();
        /** @type {HTMLButtonElement} */ (el.querySelector('.lb-delete')).onclick = () => this._delete();

        // Animate in
        requestAnimationFrame(() => el.classList.add('active'));
    },

    /** Display the current item */
    _show() {
        if (!this._overlay || this.index < 0) return;
        const generation = ++this._showGeneration;

        const item = this.items[this.index];
        const content = this._overlay.querySelector('.lightbox-content');
        const filename = this._overlay.querySelector('.lightbox-filename');
        const meta = this._overlay.querySelector('.lightbox-meta');
        const counter = this._overlay.querySelector('.lightbox-counter');

        filename.textContent = item.name;
        counter.textContent = `${this.index + 1} / ${this.items.length}`;

        // Format date
        const ts = (item.sort_date || item.created_at) * 1000;
        const dateStr = new Date(ts).toLocaleDateString(undefined, {
            year: 'numeric',
            month: 'short',
            day: 'numeric',
            hour: '2-digit',
            minute: '2-digit'
        });
        meta.textContent = `${dateStr} · ${item.size_formatted || ''}`;

        // Update nav button visibility
        /** @type {HTMLButtonElement} */ (this._overlay.querySelector('.lightbox-prev')).classList.toggle('hidden', !(this.index > 0));
        /** @type {HTMLButtonElement} */ (this._overlay.querySelector('.lightbox-next')).classList.toggle('hidden', !(this.index < this.items.length - 1));

        // Reset the full-resolution button for the new item
        const fullResBtn = /** @type {HTMLButtonElement} */ (this._overlay.querySelector('.lb-fullres'));
        fullResBtn.classList.add('hidden');
        fullResBtn.disabled = false;
        const fullResIcon = fullResBtn.querySelector('i');
        if (fullResIcon) fullResIcon.className = 'fas fa-expand';

        // Load content
        content.innerHTML = '<div class="photos-loading"><i class="fas fa-spinner"></i></div>';

        if (item.mime_type?.startsWith('video/')) {
            const video = document.createElement('video');
            video.controls = true;
            video.autoplay = true;
            // Instant first frame while metadata loads; a 204 (no cached
            // thumbnail yet) simply leaves the poster blank.
            video.poster = this._thumbUrl(item);
            video.src = this._originalUrl(item);
            video.addEventListener('error', () => {
                if (generation !== this._showGeneration) return;
                content.innerHTML = '<div class="photos-loading">Failed to load</div>';
            });
            // The native player has its own buffering UI — drop the spinner now.
            content.replaceChildren(video);
            this._loadMetadata(item.id, meta, dateStr, item.size_formatted || '');
            return;
        }

        // Photo: thumbnail first, original on demand. GIFs go straight to
        // the original — the thumbnail is a static JPEG and would lose the
        // animation.
        const isGif = item.mime_type === 'image/gif';
        let showingOriginal = isGif;
        const img = document.createElement('img');
        img.alt = item.name;

        img.addEventListener('load', () => {
            if (generation !== this._showGeneration) return;
            // First load replaces the spinner; the on-demand swap reuses the
            // already-attached element.
            if (!img.isConnected) content.replaceChildren(img);
            fullResBtn.classList.toggle('hidden', showingOriginal);
            fullResBtn.disabled = false;
            if (fullResIcon) fullResIcon.className = 'fas fa-expand';
        });

        img.addEventListener('error', () => {
            if (generation !== this._showGeneration) return;
            if (!showingOriginal) {
                // No server thumbnail (unsupported format or generation
                // failed) — fall back to the original.
                showingOriginal = true;
                img.src = this._originalUrl(item);
            } else {
                content.innerHTML = '<div class="photos-loading">Failed to load</div>';
                fullResBtn.classList.add('hidden');
            }
        });

        fullResBtn.onclick = () => {
            if (generation !== this._showGeneration || showingOriginal) return;
            showingOriginal = true;
            fullResBtn.disabled = true;
            if (fullResIcon) fullResIcon.className = 'fas fa-spinner fa-spin';
            img.src = this._originalUrl(item);
        };

        img.src = showingOriginal ? this._originalUrl(item) : this._thumbUrl(item);

        // Load EXIF metadata
        this._loadMetadata(item.id, meta, dateStr, item.size_formatted || '');
    },

    /**
     * Load EXIF metadata for info bar
     * @param {string} fileId
     * @param {Element} metaEl
     * @param {string} dateStr
     * @param {string} sizeStr
     */
    async _loadMetadata(fileId, metaEl, dateStr, sizeStr) {
        try {
            const res = await fetch(`/api/files/${fileId}/metadata`, {
                credentials: 'include',
                headers: this._headers()
            });
            if (res.ok) {
                const metadata = /** @type {FileMetadata} */ (await res.json());
                const parts = [dateStr];
                if (sizeStr) parts.push(sizeStr);
                if (metadata.camera_make || metadata.camera_model) {
                    parts.push([metadata.camera_make, metadata.camera_model].filter(Boolean).join(' '));
                }
                if (metadata.width && metadata.height) {
                    parts.push(`${metadata.width}×${metadata.height}`);
                }
                metaEl.textContent = parts.join(' · ');

                //TODO: add geoloc pointer to openstreetmap ?
            }
        } catch (_err) {
            // Non-critical, keep existing meta
        }
    },

    /** Download current item */
    _download() {
        const item = this.items[this.index];
        if (!item) return;
        const a = document.createElement('a');
        a.href = `/api/files/${item.id}`;
        a.download = item.name;
        document.body.appendChild(a);
        a.click();
        a.remove();
    },

    /** Toggle favorite on current item */
    async _toggleFavorite() {
        const item = this.items[this.index];
        if (!item || !favorites) return;
        try {
            await fetch(`/api/favorites/file/${item.id}`, {
                method: 'POST',
                credentials: 'include',
                headers: this._headers()
            });
            const btn = this._overlay.querySelector('.lb-favorite');
            if (btn) {
                btn.classList.toggle('active');
                const icon = btn.querySelector('i');
                if (icon) {
                    icon.className = btn.classList.contains('active') ? 'fas fa-star' : 'far fa-star';
                }
            }
        } catch (err) {
            console.error('Favorite toggle failed:', err);
        }
    },

    /** Delete current item */
    async _delete() {
        const item = this.items[this.index];
        if (!item) return;
        if (!confirm(`Delete ${item.name}?`)) return;

        try {
            await fetch(`/api/files/${item.id}`, {
                method: 'DELETE',
                credentials: 'include',
                headers: this._headers()
            });
            // Remove from photosView items too
            if (this._photosView) {
                this._photosView.items = this._photosView.items.filter((f) => f.id !== item.id);
            }
            this.items.splice(this.index, 1);
            if (this.items.length === 0) {
                this.close();
                if (this._photosView) this._photosView._renderFull(); // will call renderEmpty() on this case
            } else {
                if (this.index >= this.items.length) this.index = this.items.length - 1;
                this._show();
                if (this._photosView) this._photosView._renderFull();
            }
        } catch (err) {
            console.error('Delete failed:', err);
        }
    },

    /** Keyboard navigation */
    _bindKeys() {
        this._keyHandler = (e) => {
            if (e.key === 'Escape') this.close();
            else if (e.key === 'ArrowLeft') this.prev();
            else if (e.key === 'ArrowRight') this.next();
        };
        document.addEventListener('keydown', this._keyHandler);
    },

    _unbindKeys() {
        if (this._keyHandler) {
            document.removeEventListener('keydown', this._keyHandler);
            this._keyHandler = null;
        }
    }
};
