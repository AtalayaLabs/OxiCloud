/**
 * OxiCloud - UI file type helpers
 * Isolated icon and preview classification helpers used by ui.js.
 */

import { isTextViewable } from '../core/formatters.js';

/** @import {FileInfo} from '../core/types.js' */

/** @type {Record<string, string>} */
const ICON_CLASS_MAP = {
    pdf: 'fas fa-file-pdf',
    doc: 'fas fa-file-word',
    docx: 'fas fa-file-word',
    txt: 'fas fa-file-alt',
    rtf: 'fas fa-file-alt',
    odt: 'fas fa-file-alt',
    xls: 'fas fa-file-excel',
    xlsx: 'fas fa-file-excel',
    csv: 'fas fa-file-excel',
    ods: 'fas fa-file-excel',
    ppt: 'fas fa-file-powerpoint',
    pptx: 'fas fa-file-powerpoint',
    odp: 'fas fa-file-powerpoint',
    jpg: 'fas fa-file-image',
    jpeg: 'fas fa-file-image',
    png: 'fas fa-file-image',
    gif: 'fas fa-file-image',
    svg: 'fas fa-file-image',
    webp: 'fas fa-file-image',
    bmp: 'fas fa-file-image',
    ico: 'fas fa-file-image',
    mp4: 'fas fa-file-video',
    avi: 'fas fa-file-video',
    mov: 'fas fa-file-video',
    mkv: 'fas fa-file-video',
    webm: 'fas fa-file-video',
    flv: 'fas fa-file-video',
    mp3: 'fas fa-file-audio',
    wav: 'fas fa-file-audio',
    ogg: 'fas fa-file-audio',
    flac: 'fas fa-file-audio',
    aac: 'fas fa-file-audio',
    m4a: 'fas fa-file-audio',
    zip: 'fas fa-file-archive',
    rar: 'fas fa-file-archive',
    '7z': 'fas fa-file-archive',
    tar: 'fas fa-file-archive',
    gz: 'fas fa-file-archive',
    js: 'fas fa-file-code',
    ts: 'fas fa-file-code',
    py: 'fas fa-file-code',
    rs: 'fas fa-file-code',
    java: 'fas fa-file-code',
    html: 'fas fa-file-code',
    css: 'fas fa-file-code',
    json: 'fas fa-file-code',
    xml: 'fas fa-file-code',
    sh: 'fas fa-terminal',
    bash: 'fas fa-terminal',
    bat: 'fas fa-terminal',
    md: 'fas fa-file-alt'
};

/** @type {Record<string, string>} */
const ICON_SPECIAL_CLASS_MAP = {
    pdf: 'pdf-icon',
    doc: 'doc-icon',
    docx: 'doc-icon',
    odt: 'doc-icon',
    rtf: 'doc-icon',
    xls: 'spreadsheet-icon',
    xlsx: 'spreadsheet-icon',
    ods: 'spreadsheet-icon',
    csv: 'spreadsheet-icon',
    ppt: 'presentation-icon',
    pptx: 'presentation-icon',
    odp: 'presentation-icon',
    key: 'presentation-icon',
    jpg: 'image-icon',
    jpeg: 'image-icon',
    png: 'image-icon',
    gif: 'image-icon',
    svg: 'image-icon',
    webp: 'image-icon',
    bmp: 'image-icon',
    ico: 'image-icon',
    heic: 'image-icon',
    heif: 'image-icon',
    avif: 'image-icon',
    tiff: 'image-icon',
    mp4: 'video-icon',
    avi: 'video-icon',
    mkv: 'video-icon',
    mov: 'video-icon',
    wmv: 'video-icon',
    flv: 'video-icon',
    webm: 'video-icon',
    m4v: 'video-icon',
    mp3: 'audio-icon',
    wav: 'audio-icon',
    ogg: 'audio-icon',
    flac: 'audio-icon',
    aac: 'audio-icon',
    wma: 'audio-icon',
    m4a: 'audio-icon',
    opus: 'audio-icon',
    zip: 'archive-icon',
    rar: 'archive-icon',
    '7z': 'archive-icon',
    tar: 'archive-icon',
    gz: 'archive-icon',
    bz2: 'archive-icon',
    xz: 'archive-icon',
    exe: 'installer-icon',
    msi: 'installer-icon',
    dmg: 'installer-icon',
    deb: 'installer-icon',
    rpm: 'installer-icon',
    appimage: 'installer-icon',
    py: 'code-icon py-icon',
    rs: 'code-icon rust-icon',
    go: 'code-icon go-icon',
    js: 'code-icon js-icon',
    jsx: 'code-icon js-icon',
    mjs: 'code-icon js-icon',
    ts: 'code-icon ts-icon',
    tsx: 'code-icon ts-icon',
    java: 'code-icon java-icon',
    c: 'code-icon c-icon',
    cpp: 'code-icon c-icon',
    cs: 'code-icon cs-icon',
    rb: 'code-icon ruby-icon',
    php: 'code-icon php-icon',
    swift: 'code-icon swift-icon',
    html: 'code-icon html-icon',
    htm: 'code-icon html-icon',
    css: 'code-icon css-icon',
    scss: 'code-icon css-icon',
    json: 'code-icon json-icon',
    xml: 'code-icon html-icon',
    yaml: 'code-icon config-icon',
    yml: 'code-icon config-icon',
    toml: 'code-icon config-icon',
    ini: 'code-icon config-icon',
    sql: 'code-icon sql-icon',
    vue: 'code-icon js-icon',
    svelte: 'code-icon js-icon',
    sh: 'script-icon',
    bash: 'script-icon',
    zsh: 'script-icon',
    bat: 'script-icon',
    md: 'code-icon md-icon',
    txt: 'doc-icon'
};

const uiFileTypes = {
    // TODO: 'd better to use a canViw() method in inlineViewer
    /**
     *
     * @param {FileInfo} file
     * @returns {boolean}
     */
    isViewableFile(file) {
        if (!file?.mime_type) return false;
        if (file.mime_type.startsWith('image/')) return true;
        if (file.mime_type === 'application/pdf') return true;
        if (file.mime_type.startsWith('audio/')) return true;
        if (file.mime_type.startsWith('video/')) return true;
        return isTextViewable(file.mime_type);
    },

    /**
     *
     * @param {string} fileName
     * @returns {string}
     */
    getIconClass(fileName) {
        if (!fileName) return 'fas fa-file';
        const ext = (fileName.split('.').pop() || '').toLowerCase();

        return ICON_CLASS_MAP[ext] || 'fas fa-file';
    },

    /**
     *
     * @param {string} fileName
     * @returns
     */
    getIconSpecialClass(fileName) {
        if (!fileName) return '';
        const ext = (fileName.split('.').pop() || '').toLowerCase();

        return ICON_SPECIAL_CLASS_MAP[ext] || '';
    }
};

export { uiFileTypes };
