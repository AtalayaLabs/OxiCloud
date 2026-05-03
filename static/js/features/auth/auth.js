/**
 * OxiCloud Authentication JavaScript
 * Handles login, registration, and admin setup
 */

import { getCsrfHeaders } from '../../core/csrf.js';
import { i18n } from '../../core/i18n.js';

// API endpoints
const API_URL = '/api/auth';
const LOGIN_ENDPOINT = `${API_URL}/login`;
const REGISTER_ENDPOINT = `${API_URL}/register`;
const ME_ENDPOINT = `${API_URL}/me`;
const REFRESH_ENDPOINT = `${API_URL}/refresh`;

// Storage keys — tokens are now in HttpOnly cookies (set by server).
// Only non-sensitive display data is kept in localStorage.
const USER_DATA_KEY = 'oxicloud_user';
const LOCALE_KEY = 'oxicloud-locale';
const FIRST_RUN_KEY = 'oxicloud_first_run_completed';

// Language selector texts (used before i18n is loaded)
const LANGUAGE_TEXTS = {
    en: {
        title: 'Welcome!',
        subtitle: 'Select your language to continue',
        continue: 'Continue',
        autodetected: 'We detected your language',
        moreLanguages: 'More languages...',
        modalTitle: 'Select language',
        searchPlaceholder: 'Search language...'
    },
    es: {
        title: '¡Bienvenido!',
        subtitle: 'Selecciona tu idioma para continuar',
        continue: 'Continuar',
        autodetected: 'Hemos detectado tu idioma',
        moreLanguages: 'More languages...',
        modalTitle: 'Seleccionar idioma',
        searchPlaceholder: 'Buscar idioma...'
    },
    zh: {
        title: '欢迎！',
        subtitle: '选择您的语言以继续',
        continue: '继续',
        autodetected: '我们检测到了您的语言',
        moreLanguages: '更多语言...',
        modalTitle: '选择语言',
        searchPlaceholder: '搜索语言...'
    },
    fa: {
        title: '!خوش آمدید',
        subtitle: 'زبان خود را برای ادامه انتخاب کنید',
        continue: 'ادامه',
        autodetected: 'زبان شما شناسایی شد',
        moreLanguages: 'زبان‌های بیشتر...',
        modalTitle: 'انتخاب زبان',
        searchPlaceholder: 'جستجوی زبان...'
    },
    nl: {
        title: 'Welkom!',
        subtitle: 'Selecteer uw taal om door te gaan',
        continue: 'Doorgaan',
        autodetected: 'We hebben uw taal gedetecteerd',
        moreLanguages: 'Meer talen...',
        modalTitle: 'Taal selecteren',
        searchPlaceholder: 'Zoek taal...'
    },
    hi: {
        title: 'स्वागत है!',
        subtitle: 'जारी रखने के लिए अपनी भाषा चुनें',
        continue: 'जारी रखें',
        autodetected: 'हमने आपकी भाषा पहचान ली',
        moreLanguages: 'और भाषाएँ...',
        modalTitle: 'भाषा चुनें',
        searchPlaceholder: 'भाषा खोजें...'
    },
    ar: {
        title: '!مرحباً',
        subtitle: 'اختر لغتك للمتابعة',
        continue: 'متابعة',
        autodetected: 'تم اكتشاف لغتك',
        moreLanguages: 'المزيد من اللغات...',
        modalTitle: 'اختر اللغة',
        searchPlaceholder: 'ابحث عن لغة...'
    },
    ru: {
        title: 'Добро пожаловать!',
        subtitle: 'Выберите язык для продолжения',
        continue: 'Продолжить',
        autodetected: 'Мы определили ваш язык',
        moreLanguages: 'Больше языков...',
        modalTitle: 'Выберите язык',
        searchPlaceholder: 'Поиск языка...'
    },
    ja: {
        title: 'ようこそ！',
        subtitle: '続行するには言語を選択してください',
        continue: '続行',
        autodetected: '言語を検出しました',
        moreLanguages: 'その他の言語...',
        modalTitle: '言語を選択',
        searchPlaceholder: '言語を検索...'
    },
    ko: {
        title: '환영합니다!',
        subtitle: '계속하려면 언어를 선택하세요',
        continue: '계속',
        autodetected: '언어가 감지되었습니다',
        moreLanguages: '더 많은 언어...',
        modalTitle: '언어 선택',
        searchPlaceholder: '언어 검색...'
    }
};

// Complete language registry — add new languages here, they'll appear automatically
// `popular: true` languages show as cards on the main screen, the rest in the modal
export const ALL_LANGUAGES = [
    {
        code: 'en',
        name: 'English',
        nativeName: 'English',
        flag: '🇬🇧',
        popular: true
    },
    {
        code: 'es',
        name: 'Spanish',
        nativeName: 'Español',
        flag: '🇪🇸',
        popular: true
    },
    {
        code: 'zh',
        name: 'Chinese',
        nativeName: '中文',
        flag: '🇨🇳',
        popular: true
    },
    {
        code: 'fa',
        name: 'Persian',
        nativeName: 'فارسی',
        flag: '🇮🇷',
        popular: true
    },
    {
        code: 'fr',
        name: 'French',
        nativeName: 'Français',
        flag: '🇫🇷',
        popular: true
    },
    {
        code: 'de',
        name: 'German',
        nativeName: 'Deutsch',
        flag: '🇩🇪',
        popular: true
    },
    {
        code: 'pt',
        name: 'Portuguese',
        nativeName: 'Português',
        flag: '🇧🇷',
        popular: true
    },
    {
        code: 'it',
        name: 'Italian',
        nativeName: 'Italiano',
        flag: '🇮🇹',
        popular: true
    },
    {
        code: 'ru',
        name: 'Russian',
        nativeName: 'Русский',
        flag: '🇷🇺',
        popular: true
    },
    {
        code: 'ja',
        name: 'Japanese',
        nativeName: '日本語',
        flag: '🇯🇵',
        popular: true
    },
    {
        code: 'ko',
        name: 'Korean',
        nativeName: '한국어',
        flag: '🇰🇷',
        popular: true
    },
    {
        code: 'ar',
        name: 'Arabic',
        nativeName: 'العربية',
        flag: '🇸🇦',
        popular: true
    },
    { code: 'hi', name: 'Hindi', nativeName: 'हिन्दी', flag: '🇮🇳', popular: true },
    {
        code: 'tr',
        name: 'Turkish',
        nativeName: 'Türkçe',
        flag: '🇹🇷',
        popular: false
    },
    {
        code: 'nl',
        name: 'Dutch',
        nativeName: 'Nederlands',
        flag: '🇳🇱',
        popular: false
    },
    {
        code: 'pl',
        name: 'Polish',
        nativeName: 'Polski',
        flag: '🇵🇱',
        popular: false
    },
    {
        code: 'sv',
        name: 'Swedish',
        nativeName: 'Svenska',
        flag: '🇸🇪',
        popular: false
    },
    {
        code: 'da',
        name: 'Danish',
        nativeName: 'Dansk',
        flag: '🇩🇰',
        popular: false
    },
    {
        code: 'fi',
        name: 'Finnish',
        nativeName: 'Suomi',
        flag: '🇫🇮',
        popular: false
    },
    {
        code: 'no',
        name: 'Norwegian',
        nativeName: 'Norsk',
        flag: '🇳🇴',
        popular: false
    },
    {
        code: 'uk',
        name: 'Ukrainian',
        nativeName: 'Українська',
        flag: '🇺🇦',
        popular: false
    },
    {
        code: 'cs',
        name: 'Czech',
        nativeName: 'Čeština',
        flag: '🇨🇿',
        popular: false
    },
    {
        code: 'el',
        name: 'Greek',
        nativeName: 'Ελληνικά',
        flag: '🇬🇷',
        popular: false
    },
    {
        code: 'he',
        name: 'Hebrew',
        nativeName: 'עברית',
        flag: '🇮🇱',
        popular: false
    },
    { code: 'th', name: 'Thai', nativeName: 'ไทย', flag: '🇹🇭', popular: false },
    {
        code: 'vi',
        name: 'Vietnamese',
        nativeName: 'Tiếng Việt',
        flag: '🇻🇳',
        popular: false
    },
    {
        code: 'id',
        name: 'Indonesian',
        nativeName: 'Bahasa Indonesia',
        flag: '🇮🇩',
        popular: false
    },
    {
        code: 'ms',
        name: 'Malay',
        nativeName: 'Bahasa Melayu',
        flag: '🇲🇾',
        popular: false
    },
    {
        code: 'ro',
        name: 'Romanian',
        nativeName: 'Română',
        flag: '🇷🇴',
        popular: false
    },
    {
        code: 'hu',
        name: 'Hungarian',
        nativeName: 'Magyar',
        flag: '🇭🇺',
        popular: false
    },
    {
        code: 'ca',
        name: 'Catalan',
        nativeName: 'Català',
        flag: '🏴',
        popular: false
    },
    {
        code: 'eu',
        name: 'Basque',
        nativeName: 'Euskara',
        flag: '🏴',
        popular: false
    },
    {
        code: 'gl',
        name: 'Galician',
        nativeName: 'Galego',
        flag: '🏴',
        popular: false
    }
];

// --- Panel visibility helpers ---
// The `.hidden` CSS class uses `display: none !important`, so inline
// `style.display` can never override it.  Always toggle the class instead.
function showPanel(el) {
    if (el) el.classList.remove('hidden');
}
function hidePanel(el) {
    if (el) el.classList.add('hidden');
}

// Check if this is a first run (no locale saved)
function isFirstRun() {
    return !localStorage.getItem(LOCALE_KEY);
}

// Check system status from the server
async function checkSystemStatus() {
    try {
        const response = await fetch('/api/auth/status');
        if (!response.ok) {
            console.warn('Could not check system status, assuming initialized');
            return { initialized: true, admin_count: 1, registration_allowed: true };
        }
        return await response.json();
    } catch (error) {
        console.error('Error checking system status:', error);
        return { initialized: true, admin_count: 1, registration_allowed: true };
    }
}

// Detect user's browser language and return the best matching language from ALL_LANGUAGES
function detectBrowserLanguage() {
    const browserLangs = navigator.languages || [navigator.language || navigator.userLanguage || 'en'];
    for (const bl of browserLangs) {
        const code = bl.substring(0, 2).toLowerCase();
        const match = ALL_LANGUAGES.find((l) => l.code === code);
        if (match) return match;
    }
    return ALL_LANGUAGES[0]; // fallback to English
}

// Build a language option element (card style)
function buildLanguageCard(lang, isSelected) {
    const item = document.createElement('div');
    item.className = `lang-picker-item${isSelected ? ' selected' : ''}`;
    item.setAttribute('data-lang', lang.code);
    item.setAttribute('role', 'option');
    item.setAttribute('aria-selected', isSelected);
    item.innerHTML = `
        <span class="lang-picker-item-flag">${lang.flag}</span>
        <span class="lang-picker-item-name">${lang.nativeName}</span>
        <span class="lang-picker-item-english">${lang.name}</span>
        ${isSelected ? '<i class="fas fa-check lang-picker-item-check"></i>' : ''}
    `;
    return item;
}

// Initialize language selector panel with compact dropdown approach
function initLanguageSelector() {
    const languagePanel = document.getElementById('language-panel');
    const continueBtn = document.getElementById('language-continue');
    const picker = document.getElementById('lang-picker');
    const pickerSelected = document.getElementById('lang-picker-selected');
    const pickerList = document.getElementById('lang-picker-list');
    const pickerFlag = document.getElementById('lang-picker-flag');
    const pickerName = document.getElementById('lang-picker-name');
    const searchInput = document.getElementById('lang-picker-search-input');

    if (!languagePanel || !picker || !pickerFlag || !pickerName || !pickerList) return;

    // --- Auto-detect browser language ---
    const detected = detectBrowserLanguage();
    let selectedLanguage = detected.code;

    // Update the selected box with detected language
    pickerFlag.textContent = detected.flag;
    pickerName.textContent = detected.nativeName;
    updateLanguagePanelTexts(selectedLanguage);

    // Render dropdown list
    function renderDropdownList(filter = '') {
        pickerList.innerHTML = '';
        const filterLower = filter.toLowerCase();

        const filtered = ALL_LANGUAGES.filter((lang) => {
            if (!filter) return true;
            return (
                lang.name.toLowerCase().includes(filterLower) ||
                lang.nativeName.toLowerCase().includes(filterLower) ||
                lang.code.toLowerCase().includes(filterLower)
            );
        });

        if (filtered.length === 0) {
            pickerList.innerHTML = '<div class="lang-picker-empty">—</div>';
            return;
        }

        filtered.forEach((lang) => {
            const item = buildLanguageCard(lang, lang.code === selectedLanguage);
            item.addEventListener('click', (e) => {
                e.stopPropagation();
                selectedLanguage = lang.code;
                pickerFlag.textContent = lang.flag;
                pickerName.textContent = lang.nativeName;
                updateLanguagePanelTexts(lang.code);
                closePicker();
                renderDropdownList('');
            });
            pickerList.appendChild(item);
        });
    }

    function openPicker() {
        picker.classList.add('open');
        pickerSelected.setAttribute('aria-expanded', 'true');
        renderDropdownList('');
        if (searchInput) {
            searchInput.value = '';
            setTimeout(() => searchInput.focus(), 50);
        }
        // Scroll active item into view
        setTimeout(() => {
            const active = pickerList.querySelector('.lang-picker-item.selected');
            if (active) active.scrollIntoView({ block: 'nearest' });
        }, 60);
    }

    function closePicker() {
        picker.classList.remove('open');
        pickerSelected.setAttribute('aria-expanded', 'false');
        if (searchInput) searchInput.value = '';
    }

    // Toggle dropdown
    pickerSelected.addEventListener('click', (e) => {
        e.stopPropagation();
        if (picker.classList.contains('open')) {
            closePicker();
        } else {
            openPicker();
        }
    });

    // Keyboard support
    pickerSelected.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            if (picker.classList.contains('open')) closePicker();
            else openPicker();
        } else if (e.key === 'Escape') {
            closePicker();
        }
    });

    // Search input
    if (searchInput) {
        searchInput.addEventListener('input', () => renderDropdownList(searchInput.value));
        searchInput.addEventListener('click', (e) => e.stopPropagation());
    }

    // Close when clicking outside
    document.addEventListener('click', (e) => {
        if (!picker.contains(e.target)) closePicker();
    });

    // --- Continue button ---
    continueBtn.addEventListener('click', async () => {
        if (!selectedLanguage) return;

        // Save locale preference
        localStorage.setItem(LOCALE_KEY, selectedLanguage);
        localStorage.setItem(FIRST_RUN_KEY, 'true');

        await i18n.setLocale(selectedLanguage);

        // Hide language panel
        hidePanel(languagePanel);

        // Check system status to determine which panel to show
        const systemStatus = await checkSystemStatus();
        console.log('System status after language selection:', systemStatus);

        if (!systemStatus.initialized) {
            console.log('No admin exists, showing admin setup panel');
            hidePanel(document.getElementById('login-panel'));
            hidePanel(document.getElementById('register-panel'));
            showPanel(document.getElementById('admin-setup-panel'));

            const backToLoginLink = document.getElementById('back-to-login');
            if (backToLoginLink) {
                backToLoginLink.parentElement.style.display = 'none';
            }
        } else {
            showPanel(document.getElementById('login-panel'));
            // Configure OIDC login UI if SSO is enabled
            await configureOidcLoginUI();
        }

        // setLocale() already calls translatePage() internally
    });
}

// Update language panel texts based on selected language
function updateLanguagePanelTexts(lang) {
    const texts = LANGUAGE_TEXTS[lang] || LANGUAGE_TEXTS.en;
    const titleEl = document.getElementById('language-title');
    const subtitleEl = document.getElementById('language-subtitle');
    const continueBtn = document.getElementById('language-continue');
    const searchInput = document.getElementById('lang-picker-search-input');

    if (titleEl) titleEl.textContent = texts.title;
    if (subtitleEl) subtitleEl.textContent = texts.subtitle;
    if (continueBtn) continueBtn.textContent = texts.continue;
    if (searchInput) searchInput.placeholder = texts.searchPlaceholder;
}

// Show appropriate panel based on system status and first run
async function showInitialPanel() {
    const languagePanel = document.getElementById('language-panel');
    const loginPanel = document.getElementById('login-panel');
    const adminSetupPanel = document.getElementById('admin-setup-panel');
    const registerPanel = document.getElementById('register-panel');

    if (!languagePanel || !loginPanel) return;

    // ALWAYS check if this is user's first run (language selection) FIRST
    // Language selection should happen before anything else
    if (isFirstRun()) {
        // First run - show language selector first
        // After language is selected, the continue button handler will check system status
        console.log('First run - showing language selector');
        showPanel(languagePanel);
        hidePanel(loginPanel);
        hidePanel(registerPanel);
        hidePanel(adminSetupPanel);
        return;
    }

    // Language already selected - now check system status
    const systemStatus = await checkSystemStatus();
    console.log('System status:', systemStatus);

    if (!systemStatus.initialized) {
        // No admin exists - this is a fresh install, show admin setup
        console.log('Fresh install detected - showing admin setup');
        hidePanel(languagePanel);
        hidePanel(loginPanel);
        hidePanel(registerPanel);
        showPanel(adminSetupPanel);

        // Hide the "Already set up? Sign in" link since there's no admin yet
        const backToLoginLink = document.getElementById('back-to-login');
        if (backToLoginLink) {
            backToLoginLink.parentElement.style.display = 'none';
        }
        return;
    }

    // System is initialized - show login panel
    hidePanel(languagePanel);
    showPanel(loginPanel);
    hidePanel(registerPanel);
    hidePanel(adminSetupPanel);

    // Hide the admin setup link if admin already exists
    const showAdminSetupLink = document.getElementById('show-admin-setup');
    if (showAdminSetupLink && systemStatus.admin_count > 0) {
        showAdminSetupLink.parentElement.style.display = 'none';
    }

    // Check for OIDC/SSO configuration and update login panel accordingly
    await configureOidcLoginUI();
}

// Fetch OIDC provider info and configure the login UI
async function configureOidcLoginUI() {
    try {
        const response = await fetch('/api/auth/oidc/providers');
        if (!response.ok) return;

        const oidcInfo = await response.json();
        if (!oidcInfo.enabled) return;

        const oidcSection = document.getElementById('oidc-login-section');
        const oidcBtn = document.getElementById('oidc-login-btn');
        const loginForm = document.getElementById('login-form');
        const authDivider = document.getElementById('auth-divider');
        const showRegisterToggle = document.getElementById('show-register');

        if (!oidcSection || !oidcBtn) return;

        // Update button text with provider name
        const btnTextEl = oidcBtn.querySelector('span');
        if (btnTextEl && oidcInfo.provider_name) {
            const template = i18n.t('auth.sso_login_provider');
            btnTextEl.textContent = template.replace('{{provider}}', oidcInfo.provider_name);
        }

        // Redirect to OIDC authorize endpoint on click
        oidcBtn.addEventListener('click', () => {
            window.location.href = oidcInfo.authorize_endpoint;
        });

        if (!oidcInfo.password_login_enabled) {
            // OIDC-only mode: hide password form and divider, show only SSO button
            if (loginForm) loginForm.style.display = 'none';
            if (authDivider) authDivider.style.display = 'none';
            if (showRegisterToggle) showRegisterToggle.parentElement.style.display = 'none';
            showPanel(oidcSection);
        } else {
            // Both password and OIDC enabled: show divider + SSO button
            showPanel(oidcSection);
        }
    } catch (err) {
        console.error('Failed to fetch OIDC provider info:', err);
    }
}

// DOM elements
let loginPanel, registerPanel, adminSetupPanel;
let loginForm, registerForm, adminSetupForm;
let loginError, registerError, registerSuccess, adminSetupError;

// Initialize DOM elements only if we're on the login page
function initLoginElements() {
    // Check if we're on the login page
    if (!document.getElementById('login-form')) {
        console.log('Not on login page, skipping element initialization');
        return false;
    }

    loginPanel = document.getElementById('login-panel');
    registerPanel = document.getElementById('register-panel');
    adminSetupPanel = document.getElementById('admin-setup-panel');

    loginForm = document.getElementById('login-form');
    registerForm = document.getElementById('register-form');
    adminSetupForm = document.getElementById('admin-setup-form');

    loginError = document.getElementById('login-error');
    registerError = document.getElementById('register-error');
    registerSuccess = document.getElementById('register-success');
    adminSetupError = document.getElementById('admin-setup-error');

    // Initialize language selector
    initLanguageSelector();

    // Panel toggles
    document.getElementById('show-register').addEventListener('click', () => {
        hidePanel(loginPanel);
        showPanel(registerPanel);
        hidePanel(adminSetupPanel);
    });

    document.getElementById('show-login').addEventListener('click', () => {
        showPanel(loginPanel);
        hidePanel(registerPanel);
        hidePanel(adminSetupPanel);
    });

    document.getElementById('show-admin-setup').addEventListener('click', () => {
        hidePanel(loginPanel);
        hidePanel(registerPanel);
        showPanel(adminSetupPanel);
    });

    document.getElementById('back-to-login').addEventListener('click', () => {
        showPanel(loginPanel);
        hidePanel(registerPanel);
        hidePanel(adminSetupPanel);
    });

    return true;
}

// Initialize login elements if on login page
const isLoginPage = initLoginElements();

// Check if we already have a valid token
let authInitialized = false;

// EMERGENCY HANDLER: Detect if page is being loaded from a redirect loop
// and clear auth data to break the loop
(() => {
    // Check if we're being redirected in a loop
    const refreshAttempts = parseInt(localStorage.getItem('refresh_attempts') || '0', 10);
    const redirectSource = new URLSearchParams(window.location.search).get('source');

    // Case 1: High refresh attempts
    if (refreshAttempts > 3) {
        console.error('EMERGENCY: Detected severe token refresh loop. Cleaning all auth data.');
        localStorage.removeItem(USER_DATA_KEY);
        sessionStorage.clear();
        localStorage.setItem('emergency_clean', 'true');

        // Store timestamp of the cleanup for stability
        localStorage.setItem('last_emergency_clean', Date.now().toString());
    }

    // Case 2: We were redirected from app due to auth issues
    if (redirectSource === 'app') {
        console.log('Detected redirect from app, ensuring clean auth state');
        localStorage.removeItem(USER_DATA_KEY);
        // Reset counters
        sessionStorage.removeItem('redirect_count');
        localStorage.setItem('refresh_attempts', '0');
    }

    // Case 3: Multiple redirects in short time
    const lastCleanup = parseInt(localStorage.getItem('last_emergency_clean') || '0', 10);
    const timeSinceCleanup = Date.now() - lastCleanup;

    if (lastCleanup > 0 && timeSinceCleanup < 10000) {
        // Less than 10 seconds
        console.warn('Multiple auth problems in short time, clearing auth data');
        localStorage.removeItem(USER_DATA_KEY);
    }
})();

document.addEventListener('DOMContentLoaded', () => {
    // CRITICAL: Stop any potential redirect loops by handling browser throttling
    if (document.visibilityState === 'hidden') {
        console.warn('Page hidden, avoiding potential navigation loop');
        return;
    }

    // Check if we're on the login page
    if (!document.getElementById('login-form')) {
        console.log('Not on login page, skipping auth check');
        return;
    }

    // --- OIDC exchange code handling (fallback if landing on login page) ---
    const urlParams = new URLSearchParams(window.location.search);
    const oidcCode = urlParams.get('oidc_code');

    if (oidcCode) {
        console.log('OIDC exchange code detected on login page, exchanging...');
        (async () => {
            try {
                const resp = await fetch('/api/auth/oidc/exchange', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
                    body: JSON.stringify({ code: oidcCode })
                });

                if (!resp.ok) {
                    console.error('OIDC exchange failed:', resp.status);
                    return; // Fall through to normal login page
                }

                const data = await resp.json();
                // Tokens are now set as HttpOnly cookies by the server.
                // Just store user display data and redirect.
                if (data.user) {
                    localStorage.setItem(USER_DATA_KEY, JSON.stringify(data.user));
                }

                // Redirect to main app
                window.location.href = '/';
                return;
            } catch (err) {
                console.error('OIDC exchange error:', err);
            }
        })();
        return; // Don't initialize login page while exchanging
    }

    if (authInitialized) {
        console.log('Auth already initialized, skipping');
        return;
    }
    authInitialized = true;

    // Show appropriate panel (language selector on first run, login otherwise, or admin setup if no admin)
    // This is async so we call it and let it run
    showInitialPanel()
        .then(() => {
            console.log('Initial panel shown based on system status');
        })
        .catch((err) => {
            console.error('Error showing initial panel:', err);
        });

    // Always clear counters when loading the login page
    // to ensure we don't get trapped in a loop
    console.log('Login page loaded, clearing all counters');
    sessionStorage.removeItem('redirect_count');
    localStorage.removeItem('refresh_attempts');

    (async () => {
        try {
            // Check if we already have a valid session (cookie-based).
            // The HttpOnly cookie is sent automatically — just probe /api/auth/me.
            try {
                const meResp = await fetch(ME_ENDPOINT, {
                    method: 'GET',
                    credentials: 'same-origin'
                });
                if (meResp.ok) {
                    console.log('Session still valid, redirecting to app');
                    const userData = await meResp.json();
                    localStorage.setItem(USER_DATA_KEY, JSON.stringify(userData));
                    redirectToMainApp();
                    return;
                }
                // 401 / other → try a silent refresh
                console.log('Session check returned', meResp.status, '— trying refresh');
                const refreshOk = await refreshAuthToken();
                if (refreshOk) {
                    console.log('Token refresh successful, redirecting to app');
                    redirectToMainApp();
                    return;
                }
            } catch (err) {
                console.log('Session probe failed, showing login page:', err.message);
            }
            // No valid session — stay on login page
            localStorage.removeItem(USER_DATA_KEY);

            // Check if admin account exists (customize this as needed)
            const isFirstRun = await checkFirstRun();
            if (isFirstRun) {
                hidePanel(loginPanel);
                hidePanel(registerPanel);
                showPanel(adminSetupPanel);
            }
        } catch (error) {
            console.error('Authentication check failed:', error);
        }
    })();
});

// Login form submission
if (isLoginPage && loginForm) {
    loginForm.addEventListener('submit', async (e) => {
        e.preventDefault();

        // Clear previous errors
        loginError.style.display = 'none';

        const username = document.getElementById('login-username').value;
        const password = document.getElementById('login-password').value;

        try {
            const data = await login(username, password);

            // Tokens are now set as HttpOnly cookies by the server.
            // Just store non-sensitive user data for display.
            console.log('Login succeeded');

            // Reset redirect counter on successful login
            sessionStorage.removeItem('redirect_count');
            localStorage.setItem('refresh_attempts', '0');

            if (data.user) {
                localStorage.setItem(USER_DATA_KEY, JSON.stringify(data.user));
            }

            // Redirect to main app — but first verify the browser accepted
            // the auth cookies.  The CSRF cookie (oxicloud_csrf) is non-HttpOnly
            // so JS can read it.  If it's missing the browser rejected the
            // Set-Cookie (usually because of Secure flag over plain HTTP).
            const csrfStored = document.cookie.split('; ').some((c) => c.startsWith('oxicloud_csrf='));
            if (!csrfStored) {
                console.error(
                    'Auth cookies were NOT stored by the browser. ' +
                        'This usually means OXICLOUD_COOKIE_SECURE=true (or OXICLOUD_BASE_URL=https://...) ' +
                        'is set but you are accessing via plain HTTP.'
                );
                loginError.textContent =
                    'Login succeeded but the browser rejected the session cookie. ' +
                    'If you are accessing via HTTP, set OXICLOUD_COOKIE_SECURE=false in your .env file ' +
                    'or access via HTTPS through a reverse proxy.';
                loginError.style.display = 'block';
                return;
            }
            redirectToMainApp();
        } catch (error) {
            loginError.textContent = error.message || 'Error logging in';
            loginError.style.display = 'block';
        }
    });
}

// Register form submission
if (isLoginPage && registerForm) {
    registerForm.addEventListener('submit', async (e) => {
        e.preventDefault();

        // Clear previous messages
        registerError.style.display = 'none';
        registerSuccess.style.display = 'none';

        const username = document.getElementById('register-username').value;
        const email = document.getElementById('register-email').value;
        const password = document.getElementById('register-password').value;
        const confirmPassword = document.getElementById('register-password-confirm').value;

        // Validate passwords match
        if (password !== confirmPassword) {
            registerError.textContent = i18n.t('auth.passwords_mismatch');
            registerError.style.display = 'block';
            return;
        }

        try {
            await register(username, email, password);

            registerSuccess.textContent = i18n.t('auth.account_success');
            registerSuccess.style.display = 'block';

            // Clear form
            registerForm.reset();

            // Switch to login panel after 2 seconds
            setTimeout(() => {
                showPanel(loginPanel);
                hidePanel(registerPanel);
            }, 2000);
        } catch (error) {
            registerError.textContent = error.message || i18n.t('auth.admin_create_error');
            registerError.style.display = 'block';
        }
    });
}

// Admin setup form submission
if (isLoginPage && adminSetupForm) {
    adminSetupForm.addEventListener('submit', async (e) => {
        e.preventDefault();

        // Clear previous errors/success messages
        adminSetupError.style.display = 'none';
        const adminSetupSuccess = document.getElementById('admin-setup-success');
        if (adminSetupSuccess) adminSetupSuccess.style.display = 'none';

        const email = document.getElementById('admin-email').value;
        const password = document.getElementById('admin-password').value;
        const confirmPassword = document.getElementById('admin-password-confirm').value;

        // Validate passwords match
        if (password !== confirmPassword) {
            adminSetupError.textContent = i18n.t('auth.passwords_mismatch');
            adminSetupError.style.display = 'block';
            return;
        }

        try {
            // Use the /api/setup endpoint which creates an admin and marks the system as initialized
            const response = await fetch('/api/setup', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
                credentials: 'same-origin',
                body: JSON.stringify({
                    username: 'admin',
                    email,
                    password
                })
            });
            if (!response.ok) {
                const err = await response.json().catch(() => ({}));
                throw new Error(err.message || 'Setup failed');
            }
            await response.json();

            if (adminSetupSuccess) {
                adminSetupSuccess.textContent = i18n.t('auth.admin_success');
                adminSetupSuccess.style.display = 'block';
            }

            // Wait 2 seconds then switch to login panel
            setTimeout(() => {
                showPanel(loginPanel);
                hidePanel(adminSetupPanel);
                if (adminSetupSuccess) adminSetupSuccess.style.display = 'none';
            }, 2000);
        } catch (error) {
            adminSetupError.textContent = error.message || i18n.t('auth.admin_create_error');
            adminSetupError.style.display = 'block';
        }
    });
}

// API Functions

/**
 * Login with username and password
 */
async function login(username, password) {
    try {
        console.log(`Attempting to login with username: ${username}`);

        // Add better error handling with timeout
        const controller = new AbortController();
        const timeoutId = setTimeout(() => controller.abort(), 10000); // 10 second timeout

        const response = await fetch(LOGIN_ENDPOINT, {
            method: 'POST',
            credentials: 'same-origin',
            headers: {
                'Content-Type': 'application/json',
                ...getCsrfHeaders()
            },
            body: JSON.stringify({ username, password }),
            signal: controller.signal
        });

        clearTimeout(timeoutId);

        console.log(`Login response status: ${response.status}`);

        // Handle both successful and error responses
        if (!response.ok) {
            try {
                const errorData = await response.json();
                throw new Error(errorData.error || 'Authentication failed');
            } catch (_jsonError) {
                // If the error response is not valid JSON
                throw new Error(`Authentication error (${response.status}): ${response.statusText}`);
            }
        }

        // Parse the JSON response
        try {
            const data = await response.json();
            console.log('Login successful, received data');
            return data;
        } catch (jsonError) {
            console.error('Error parsing login response:', jsonError);
            throw new Error('Error processing server response');
        }
    } catch (error) {
        console.error('Login error:', error);
        throw error;
    }
}

/**
 * Register a new user
 */
async function register(username, email, password, role = 'user') {
    try {
        console.log(`Attempting to register user: ${username}`);

        const response = await fetch(REGISTER_ENDPOINT, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                ...getCsrfHeaders()
            },
            body: JSON.stringify({ username, email, password, role })
        });

        console.log(`Registration response status: ${response.status}`);

        // Handle both successful and error responses
        if (!response.ok) {
            try {
                const errorData = await response.json();
                throw new Error(errorData.error || 'Registration error');
            } catch (_jsonError) {
                // If the error response is not valid JSON
                throw new Error(`Registration error (${response.status}): ${response.statusText}`);
            }
        }

        // Parse the JSON response
        try {
            const data = await response.json();
            console.log('Registration successful, received data');
            return data;
        } catch (jsonError) {
            console.error('Error parsing registration response:', jsonError);
            throw new Error('Error processing server response');
        }
    } catch (error) {
        console.error('Registration error:', error);
        throw error;
    }
}

/**
 * Fetch current user data — relies on HttpOnly cookie (auto-sent).
 */

// biome-ignore lint/correctness/noUnusedVariables: global function
async function fetchUserData() {
    try {
        const response = await fetch(ME_ENDPOINT, {
            method: 'GET',
            credentials: 'same-origin'
        });

        if (!response.ok) {
            throw new Error('Error fetching user data');
        }

        return await response.json();
    } catch (error) {
        console.error('Error fetching user data:', error);
        throw error;
    }
}

/**
 * Refresh authentication token via the server's refresh endpoint.
 * The refresh-token cookie is sent automatically (HttpOnly, Path=/api/auth).
 * Returns true on success, false on failure.
 */
async function refreshAuthToken() {
    try {
        // Loop-breaker
        const refreshAttempts = parseInt(localStorage.getItem('refresh_attempts') || '0', 10);
        localStorage.setItem('refresh_attempts', (refreshAttempts + 1).toString());

        if (refreshAttempts > 3) {
            console.error('Refresh token loop detected, giving up');
            localStorage.removeItem(USER_DATA_KEY);
            localStorage.removeItem('refresh_attempts');
            sessionStorage.removeItem('redirect_count');
            return false;
        }

        console.log('Attempting to refresh token (cookie-based)');

        const controller = new AbortController();
        const timeoutId = setTimeout(() => controller.abort(), 5000);

        const response = await fetch(REFRESH_ENDPOINT, {
            method: 'POST',
            credentials: 'same-origin',
            headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
            body: '{}',
            signal: controller.signal
        });

        clearTimeout(timeoutId);

        if (!response.ok) {
            console.warn('Refresh failed with status:', response.status);
            return false;
        }

        const data = await response.json();

        // Store user display data if provided
        if (data.user) {
            localStorage.setItem(USER_DATA_KEY, JSON.stringify(data.user));
        }

        // Reset counters on success
        localStorage.setItem('refresh_attempts', '0');
        sessionStorage.removeItem('redirect_count');

        return true;
    } catch (error) {
        console.error('Token refresh error:', error);
        localStorage.removeItem(USER_DATA_KEY);
        localStorage.removeItem('refresh_attempts');
        sessionStorage.removeItem('redirect_count');
        return false;
    }
}

/**
 * Check if this is the first run (no admin exists)
 */
async function checkFirstRun() {
    try {
        console.log('Checking if this is first run');

        // Skip the actual check - we'll assume it's not the first run
        // This avoids making the test request that's getting 403 Forbidden

        // For development/testing we can return false to show login screen
        // or true to show admin setup screen
        return false;
    } catch (error) {
        console.error('Error checking first run:', error);
        // If there's an error, default to regular login
        return false;
    }
}

/**
 * Redirect to main application — no token check needed (cookies are opaque).
 */
function redirectToMainApp() {
    console.log('Redirecting to main application');
    try {
        localStorage.setItem('refresh_attempts', '0');
        sessionStorage.removeItem('redirect_count');
        window.location.replace('/');
    } catch (error) {
        console.error('Error during redirect:', error);
        window.location.href = '/login?error=redirect_failed';
    }
}

/**
 * Logout — tell the server to clear HttpOnly cookies, then redirect.
 */

// biome-ignore lint/correctness/noUnusedVariables: global function
async function logout() {
    try {
        await fetch('/api/auth/logout', {
            method: 'POST',
            credentials: 'same-origin',
            headers: getCsrfHeaders()
        });
    } catch (e) {
        console.warn('Logout request failed:', e);
    }
    localStorage.removeItem(USER_DATA_KEY);
    localStorage.removeItem('refresh_attempts');
    window.location.href = '/login';
}
