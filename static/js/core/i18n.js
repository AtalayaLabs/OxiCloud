/**
 * OxiCloud Internationalization (i18n) Module
 *
 * This module provides functionality for internationalization of the OxiCloud web interface.
 * It loads translations from the server and provides functions to translate keys.
 */

// Current locale code (default to browser locale if available, fallback to English)
let currentLocale = navigator.language?.substring(0, 2) || navigator.userLanguage?.substring(0, 2) || 'en';

// Supported locales (languages that have locale files on the server)
// When a locale file is not found, the system gracefully falls back to English
const supportedLocales = ['en', 'es', 'zh', 'fa', 'fr', 'de', 'pt', 'nl', 'it', 'hi', 'ar', 'ru', 'ja', 'ko', 'pl'];

// Fallback to English if locale is not supported
if (!supportedLocales.includes(currentLocale)) {
    currentLocale = 'en';
}

// Cache for translations
const translations = {};

/**
 * Load translations for a specific locale
 * @param {string} locale - The locale code to load (e.g., 'en', 'es')
 * @returns {Promise<object>} - A promise that resolves to the translations object
 */
async function loadTranslations(locale) {
    // Check if already loaded
    if (translations[locale]) {
        return translations[locale];
    }

    try {
        // Load directly from local JSON file
        const localeData = await fetch(`/locales/${locale}.json`);
        if (!localeData.ok) {
            throw new Error(`Failed to load locale file for ${locale}`);
        }

        translations[locale] = await localeData.json();
        return translations[locale];
    } catch (error) {
        console.error('Error loading translations:', error);

        // Return empty object as last resort
        translations[locale] = {};
        return translations[locale];
    }
}

/**
 * Get a nested translation value
 * @param {object} obj - The translations object
 * @param {string} path - The dot-notation path to the translation
 * @returns {string|null} - The translation value or null if not found
 */
function getNestedValue(obj, path) {
    // Try direct key match first
    if (obj && typeof obj === 'object' && path in obj) {
        const value = obj[path];
        return typeof value === 'string' ? value : null;
    }

    // Try standard dot notation for nested values
    const keys = path.split('.');
    let current = obj;

    for (const key of keys) {
        if (current && typeof current === 'object' && key in current) {
            current = current[key];
        } else {
            // Key not found in standard dotted path
            // Try a last attempt with underscore format if this is a prefix_suffix format key
            if (path.includes('_') && !path.includes('.')) {
                const [prefix, ...parts] = path.split('_');
                const suffix = parts.join('_');

                if (obj[prefix] && typeof obj[prefix] === 'object' && suffix in obj[prefix]) {
                    return obj[prefix][suffix];
                }
            }
            return null;
        }
    }

    return typeof current === 'string' ? current : null;
}

/**
 * Translate a key to the current locale
 * @param {string} key - The translation key (dot notation, e.g., 'app.title')
 * @param {object} params - Parameters to replace in the translation (e.g., {name: 'John'})
 * @returns {string} - The translated string or the key itself if not found
 */

// biome-ignore lint/correctness/noUnusedVariables: global function
function t(key, params = {}) {
    // Get translation from cache
    const localeData = translations[currentLocale];
    if (!localeData) {
        // Translation not loaded yet, return key
        console.warn(`Translations for ${currentLocale} not loaded yet`);
        return key;
    }

    // Special handling for shared_ and share_ prefixed keys
    if (key.startsWith('shared_') || key.startsWith('share_')) {
        const unprefixedKey = key.replace(/^(shared|share)_/, '');
        const prefixObj = key.startsWith('shared_') ? localeData.shared : localeData.share;

        if (prefixObj && typeof prefixObj === 'object' && unprefixedKey in prefixObj) {
            return interpolate(prefixObj[unprefixedKey], params);
        }
    }

    // Get the translation value
    let value = getNestedValue(localeData, key);

    // Compatibility aliases for legacy share.* keys used in some views
    if (!value) {
        const aliasMap = {
            'share.enablePassword': 'share.password',
            'share.enableExpiration': 'share.expiration',
            'share.notifyEmail': 'share.notifyEmailLabel',
            'share.notifyMessage': 'share.notifyMessageLabel'
        };
        const aliasKey = aliasMap[key];
        if (aliasKey) {
            value = getNestedValue(localeData, aliasKey);
        }
    }

    if (!value) {
        // Try fallback to English
        if (currentLocale !== 'en' && translations.en) {
            let fallbackValue = getNestedValue(translations.en, key);

            if (!fallbackValue) {
                const aliasMap = {
                    'share.enablePassword': 'share.password',
                    'share.enableExpiration': 'share.expiration',
                    'share.notifyEmail': 'share.notifyEmailLabel',
                    'share.notifyMessage': 'share.notifyMessageLabel'
                };
                const aliasKey = aliasMap[key];
                if (aliasKey) {
                    fallbackValue = getNestedValue(translations.en, aliasKey);
                }
            }

            if (fallbackValue) {
                return interpolate(fallbackValue, params);
            }
        }

        // Key not found, return key
        console.warn(`Translation key not found: ${key}`);
        return key;
    }

    // Replace parameters
    return interpolate(value, params);
}

/**
 * Replace parameters in a translation string
 * @param {string} text - The translation string with placeholders
 * @param {object} params - The parameters to replace
 * @returns {string} - The interpolated string
 */
function interpolate(text, params) {
    return text.replace(/{{\s*([^}]+)\s*}}/g, (_, key) => {
        return params[key.trim()] !== undefined ? params[key.trim()] : `{{${key}}}`;
    });
}

/**
 * Change the current locale
 * @param {string} locale - The locale code to switch to
 * @returns {Promise<boolean>} - A promise that resolves to true if successful
 */
async function setLocale(locale) {
    if (!supportedLocales.includes(locale)) {
        console.error(`Locale not supported: ${locale}`);
        return false;
    }

    // Load translations if not loaded yet
    if (!translations[locale]) {
        await loadTranslations(locale);
    }

    // Update current locale
    currentLocale = locale;

    // Save locale preference
    localStorage.setItem('oxicloud-locale', locale);

    // Trigger an event for components to update
    window.dispatchEvent(new CustomEvent('localeChanged', { detail: { locale } }));

    // Update all elements with data-i18n attribute
    translatePage();

    return true;
}

/**
 * Initialize the i18n system
 * @returns {Promise<void>}
 */
async function initI18n() {
    // Load saved locale preference
    const savedLocale = localStorage.getItem('oxicloud-locale');
    if (savedLocale && supportedLocales.includes(savedLocale)) {
        currentLocale = savedLocale;
    }

    // Load translations for current locale
    await loadTranslations(currentLocale);

    // Preload English translations as fallback
    if (currentLocale !== 'en') {
        await loadTranslations('en');
    }

    // Mark loaded BEFORE translatePage so safeT resolves properly
    translationsLoaded = true;

    // Translate the page
    translatePage();

    console.log(`I18n initialized with locale: ${currentLocale}`);
}

/**
 * Translate all elements with data-i18n attribute
 */
function translatePage() {
    translateElement(document);
}

/**
 * Translate only elements within a given root (scoped).
 * Use this instead of translatePage() when you know which container changed.
 * @param {Element|Document} root - The root element to search within
 */
function translateElement(root) {
    // Use safeT instead of bare t() to avoid issues when other scripts
    // (e.g. admin.js) shadow the global t() function.
    const resolve = safeT;
    const el = root || document;
    el.querySelectorAll('[data-i18n]').forEach((element) => {
        const key = element.getAttribute('data-i18n');
        element.textContent = resolve(key);
    });

    el.querySelectorAll('[data-i18n-placeholder]').forEach((element) => {
        const key = element.getAttribute('data-i18n-placeholder');
        element.placeholder = resolve(key);
    });

    el.querySelectorAll('[data-i18n-title]').forEach((element) => {
        const key = element.getAttribute('data-i18n-title');
        element.title = resolve(key);
    });
}

/**
 * Get current locale
 * @returns {string} - The current locale code
 */
function getCurrentLocale() {
    return currentLocale;
}

/**
 * Get list of supported locales
 * @returns {Array<string>} - Array of supported locale codes
 */
function getSupportedLocales() {
    return [...supportedLocales];
}

// Flag to track if translations are loaded
let translationsLoaded = false;

// Initialize when DOM is ready
document.addEventListener('DOMContentLoaded', async () => {
    await initI18n();
    // translationsLoaded already set inside initI18n
    // Dispatch an event when translations are fully loaded
    window.dispatchEvent(new Event('translationsLoaded'));
});

// Self-contained t wrapper — does NOT call the global t() because other
// scripts (e.g. admin.js) may shadow it, which would cause infinite recursion.
function safeT(key, params = {}) {
    const localeData = translations[currentLocale];
    if (!localeData) {
        // Translations not loaded yet — return humanised key suffix
        return key.split('.').pop() || key;
    }

    let value = getNestedValue(localeData, key);

    // Fallback to English
    if (!value && currentLocale !== 'en' && translations.en) {
        value = getNestedValue(translations.en, key);
    }

    if (!value) return key;
    return interpolate(value, params);
}

export const i18n = {
    t: safeT,
    setLocale,
    getCurrentLocale,
    getSupportedLocales,
    translatePage,
    translateElement,
    isLoaded: () => translationsLoaded
};
