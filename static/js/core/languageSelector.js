import { ALL_LANGUAGES } from '../features/auth/auth.js';
import { i18n } from './i18n.js';

/**
 * Language Selector Component for OxiCloud
 * Custom styled dropdown with flags
 */

// Locale files that actually exist (have full translations)
// Keep this list in sync when adding new locale JSON files
const AVAILABLE_LOCALES = new Set(['en', 'es', 'zh', 'fa', 'fr', 'de', 'pt', 'it', 'nl', 'hi', 'ar', 'ru', 'ja', 'ko']);

// Language codes, names, and flag emojis
// Only returns languages that have a real locale file
function getAvailableLanguages() {
    if (typeof ALL_LANGUAGES !== 'undefined') {
        return ALL_LANGUAGES.filter((l) => AVAILABLE_LOCALES.has(l.code)).map((l) => ({ code: l.code, name: l.nativeName, flag: l.flag }));
    }
    return [
        { code: 'en', name: 'English', flag: '🇬🇧' },
        { code: 'es', name: 'Español', flag: '🇪🇸' },
        { code: 'zh', name: '中文', flag: '🇨🇳' },
        { code: 'fa', name: 'فارسی', flag: '🇮🇷' },
        { code: 'fr', name: 'Français', flag: '🇫🇷' },
        { code: 'de', name: 'Deutsch', flag: '🇩🇪' },
        { code: 'pt', name: 'Português', flag: '🇧🇷' },
        { code: 'it', name: 'Italiano', flag: '🇮🇹' },
        { code: 'nl', name: 'Nederlands', flag: '🇳🇱' }
    ];
}

// RTL languages
const rtlLanguages = ['fa', 'ar'];

// Update HTML lang attribute and dir for RTL languages
function updateHtmlAttributes(langCode) {
    const htmlElement = document.documentElement;

    // Set lang attribute
    htmlElement.setAttribute('lang', langCode);

    // Set dir attribute for RTL languages
    if (rtlLanguages.includes(langCode)) {
        htmlElement.setAttribute('dir', 'rtl');
    } else {
        htmlElement.removeAttribute('dir');
    }
}

/**
 * Creates and initializes a custom language selector component
 * @param {string} containerId - ID of the container element
 */
function createLanguageSelector(containerId = 'language-selector') {
    // Get or create container
    let container = document.getElementById(containerId);
    if (!container) {
        console.warn(`Container with ID "${containerId}" not found, creating one.`);
        container = document.createElement('div');
        container.id = containerId;
        document.body.appendChild(container);
    }

    // Ensure container has the right class
    container.className = 'language-selector';

    // Get current language
    const languages = getAvailableLanguages();
    const currentLocale = i18n.getCurrentLocale();
    const currentLang = languages.find((l) => l.code === currentLocale) || languages[0];

    // Set initial HTML attributes
    updateHtmlAttributes(currentLocale);

    // Create toggle button
    const toggle = document.createElement('div');
    toggle.className = 'language-selector-toggle';
    toggle.setAttribute('role', 'button');
    toggle.setAttribute('aria-haspopup', 'listbox');
    toggle.setAttribute('aria-expanded', 'false');
    toggle.setAttribute('tabindex', '0');
    toggle.innerHTML = `
        <i class="fas fa-globe"></i>
        <span class="lang-code">${currentLang.code.toUpperCase()}</span>
        <i class="fas fa-chevron-down dropdown-arrow"></i>
    `;

    // Create dropdown menu
    const dropdown = document.createElement('div');
    dropdown.className = 'language-selector-dropdown';
    dropdown.setAttribute('role', 'listbox');

    // Add language options
    languages.forEach((lang) => {
        const option = document.createElement('div');
        option.className = `language-option${lang.code === currentLocale ? ' active' : ''}`;
        option.setAttribute('role', 'option');
        option.setAttribute('data-lang', lang.code);
        option.setAttribute('aria-selected', String(lang.code === currentLocale));
        option.innerHTML = `
            <span class="lang-flag">${lang.flag}</span>
            <span class="lang-name">${lang.name}</span>
            <i class="fas fa-check lang-check"></i>
        `;

        option.addEventListener('click', async (e) => {
            e.stopPropagation();
            await selectLanguage(lang.code, container);
        });

        dropdown.appendChild(option);
    });

    // Clear and build container
    container.innerHTML = '';
    container.appendChild(toggle);
    container.appendChild(dropdown);

    // Toggle dropdown on click
    toggle.addEventListener('click', (e) => {
        e.stopPropagation();
        toggleDropdown(container);
    });

    // Keyboard support
    toggle.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            toggleDropdown(container);
        } else if (e.key === 'Escape') {
            closeDropdown(container);
        }
    });

    // Close dropdown when clicking outside
    document.addEventListener('click', (e) => {
        if (!container.contains(/** @type {Node | null } */ (e.target))) {
            closeDropdown(container);
        }
    });

    // Listen for locale changes from i18n system
    window.addEventListener('localeChanged', (e) => {
        updateSelectedLanguage(e.detail.locale, container);
    });

    return container;
}

/**
 * Toggle dropdown open/closed
 */
function toggleDropdown(container) {
    const isOpen = container.classList.contains('open');
    if (isOpen) {
        closeDropdown(container);
    } else {
        openDropdown(container);
    }
}

/**
 * Open dropdown
 */
function openDropdown(container) {
    container.classList.add('open');
    const toggle = container.querySelector('.language-selector-toggle');
    if (toggle) {
        toggle.setAttribute('aria-expanded', 'true');
    }
}

/**
 * Close dropdown
 */
function closeDropdown(container) {
    container.classList.remove('open');
    const toggle = container.querySelector('.language-selector-toggle');
    if (toggle) {
        toggle.setAttribute('aria-expanded', 'false');
    }
}

/**
 * Select a language
 */
async function selectLanguage(langCode, container) {
    await i18n.setLocale(langCode);

    // Update HTML lang attribute and dir for RTL languages
    updateHtmlAttributes(langCode);

    updateSelectedLanguage(langCode, container);
    closeDropdown(container);
}

/**
 * Update the UI to reflect selected language
 */
function updateSelectedLanguage(langCode, container) {
    const languages = getAvailableLanguages();
    const lang = languages.find((l) => l.code === langCode) || languages[0];

    // Update toggle button text
    const langCodeSpan = container.querySelector('.lang-code');
    if (langCodeSpan) {
        langCodeSpan.textContent = lang.code.toUpperCase();
    }

    // Update active state on options
    const options = container.querySelectorAll('.language-option');
    options.forEach((option) => {
        const isActive = option.getAttribute('data-lang') === langCode;
        option.classList.toggle('active', isActive);
        option.setAttribute('aria-selected', isActive);
    });
}

// Create language selector when DOM is ready
document.addEventListener('DOMContentLoaded', () => {
    createLanguageSelector();
});
