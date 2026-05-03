import { getCsrfHeaders } from '../core/csrf.js';

/** @type {typeof import('../vendors/pdf.min.d.ts') | null} */
let _pdfjsLib = null;

/**
 * Lazy-loads pdf.min.mjs on first use via dynamic import so it is never
 * bundled into the IIFE (it uses top-level await which breaks IIFE wrapping).
 * @returns {Promise<typeof import('../vendors/pdf.min.d.ts')>}
 */
async function getPdfjsLib() {
    if (_pdfjsLib) return _pdfjsLib;
    _pdfjsLib = await import('/js/vendors/pdf.min.mjs');
    _pdfjsLib.GlobalWorkerOptions.workerSrc = '/js/vendors/pdf.worker.min.mjs';
    return _pdfjsLib;
}

export const thumbnail = {
    SUPPORTED_CLASS: ['image-icon', 'pdf-icon', 'video-icon'],
    /**
     *
     * @param {String} iconSpecialClass
     * @returns {boolean}
     */
    canHandle(iconSpecialClass) {
        return this.SUPPORTED_CLASS.includes(iconSpecialClass);
    },

    // TODO: use these informations from server ?
    SIZES: {
        icon: { width: 150, height: 150 },
        preview: { width: 300, height: 300 },
        large: { width: 900, height: 800 }
    },

    // note: server moved to jpeg q=80 for images
    // FORMAT: 'image/webp',
    // QUALITY: 0.85,
    FORMAT: 'image/jpeg',
    QUALITY: 0.8,

    /**
     * @typedef {Object} Size
     * @property {number} width
     * @property {number} height
     */

    /**
     *
     * @param {number} srcWidth
     * @param {number} srcHeight
     * @param {number} targetWidth
     * @param {number} targetHeight
     * @returns {Size}
     */
    computeSize(srcWidth, srcHeight, targetWidth, targetHeight) {
        const srcRatio = srcWidth / srcHeight;
        const targetRatio = targetWidth / targetHeight;
        if (srcRatio > targetRatio) {
            return { width: targetWidth, height: Math.round(targetWidth / srcRatio) };
        } else {
            return { width: Math.round(targetHeight * srcRatio), height: targetHeight };
        }
    },

    /**
     *
     * @param {ImageBitmap} bitmap
     * @param {number} targetWidth
     * @param {number} targetHeight
     * @param {ImageEncodeOptions} imageEncodeOptions
     * @returns
     */
    bitmapToBlob(bitmap, targetWidth, targetHeight, imageEncodeOptions) {
        const { width, height } = this.computeSize(bitmap.width, bitmap.height, targetWidth, targetHeight);
        const canvas = new OffscreenCanvas(width, height);
        canvas.getContext('2d')?.drawImage(bitmap, 0, 0, width, height);
        return canvas.convertToBlob(imageEncodeOptions);
    },

    /**
     *
     * @param {Blob} blob
     * @returns
     */
    blobToDataUrl(blob) {
        return new Promise((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = () => resolve(reader.result);
            reader.onerror = reject;
            reader.readAsDataURL(blob);
        });
    },

    /**
     *
     * @param {Object} file
     * @param {string} source
     * @returns
     */
    async sourceToBitmap(file, source) {
        switch (file.icon_special_class) {
            case 'image-icon': {
                const response = await fetch(source);
                if (!response.ok) throw new Error(`failed to fetch: ${response.status}`);
                const blob = await response.blob();
                return createImageBitmap(blob);
            }

            case 'pdf-icon': {
                const pdfjsLib = await getPdfjsLib();
                const pdf = await pdfjsLib.getDocument(source).promise;
                const page = await pdf.getPage(1);
                const viewport = page.getViewport({ scale: 1 });
                const canvas = document.createElement('canvas');
                canvas.width = viewport.width;
                canvas.height = viewport.height;
                await page.render({ canvasContext: canvas.getContext('2d'), viewport }).promise;
                return createImageBitmap(canvas);
            }

            case 'video-icon': {
                return new Promise((resolve, reject) => {
                    const video = document.createElement('video');
                    video.src = source;
                    video.muted = true;
                    video.preload = 'metadata';
                    video.onloadedmetadata = () => {
                        // seek to 1/3 of video to take snapshot
                        video.currentTime = video.duration / 3;
                    };
                    video.onseeked = async () => {
                        const bitmap = await createImageBitmap(video);
                        video.pause();
                        video.removeAttribute('src'); // hack to close network connection
                        video.load();
                        resolve(bitmap);
                    };
                    video.onerror = reject;
                });
            }

            default:
                throw new Error(`unknown type: ${file.icon_special_class} for file ${file.name}`);
        }
    },

    /**
     * generateThumbnail and update image
     *
     * @param {Object} file the source of the image
     * @param {(dataURL: string) => void} [onIconGenerated] the callback once thumbnail is generated
     */
    async generate(file, onIconGenerated) {
        const source = `${window.location.origin}/api/files/${file.id}`;

        const bitmap = await this.sourceToBitmap(file, source);

        const [iconBlob, previewBlob, largeBlob] = await Promise.all(
            Object.values(this.SIZES).map(({ width, height }) => this.bitmapToBlob(bitmap, width, height, { type: this.FORMAT, quality: this.QUALITY }))
        );

        if (onIconGenerated) {
            onIconGenerated(await this.blobToDataUrl(iconBlob));
        }

        await Promise.all(
            [
                ['icon', iconBlob],
                ['preview', previewBlob],
                ['large', largeBlob]
            ].map(([size, blob]) =>
                fetch(`${window.location.origin}/api/files/${file.id}/thumbnail/${size}`, {
                    method: 'PUT',
                    headers: { ...getCsrfHeaders(), 'Content-Type': this.FORMAT },
                    body: blob
                }).then((r) => console.log(`uploaded ${size} thumbnail of ${file.name}: ${r.status}`))
            )
        );
    }
};
