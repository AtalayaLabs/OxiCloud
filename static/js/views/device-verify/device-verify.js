// device-verify.js — Extracted from inline <script> in device-verify.html
import { getCsrfHeaders } from '../../core/csrf.js';

(() => {
    var API_BASE = window.location.origin;
    var codeInput = /** @type {HTMLInputElement} */ (document.getElementById('user-code'));
    var deviceInfo = document.getElementById('device-info');
    var actionButtons = document.getElementById('action-buttons');
    var errorText = document.getElementById('error-text');
    var btnApprove = document.getElementById('btn-approve');
    var btnDeny = document.getElementById('btn-deny');
    var debounceTimer = null;
    var currentCode = '';

    // Pre-fill from URL query param (?code=ABCD-1234)
    var params = new URLSearchParams(window.location.search);
    if (params.get('code')) {
        codeInput.value = params.get('code');
        lookupCode(params.get('code'));
    }

    // Auto-insert hyphen and lookup on input
    codeInput.addEventListener('input', (e) => {
        var val = e.target.value.toUpperCase().replace(/[^A-Z0-9-]/g, '');
        // Auto-insert hyphen after 4 chars
        if (val.length === 4 && val.indexOf('-') === -1) {
            val = `${val}-`;
        }
        e.target.value = val;
        errorText.classList.add('hidden');

        // Debounce lookup
        clearTimeout(debounceTimer);
        if (val.length >= 9) {
            debounceTimer = setTimeout(() => {
                lookupCode(val);
            }, 300);
        } else {
            deviceInfo.classList.add('hidden');
            actionButtons.classList.add('hidden');
        }
    });

    // Wire up approve / deny buttons (replaces inline onclick)
    btnApprove.addEventListener('click', () => {
        handleAction('approve');
    });
    btnDeny.addEventListener('click', () => {
        handleAction('deny');
    });

    async function lookupCode(code) {
        try {
            const resp = await fetch(`${API_BASE}/api/auth/device/verify?code=${encodeURIComponent(code)}`, {
                credentials: 'same-origin'
            });
            if (resp.status === 401) {
                showError('You must be logged in to authorize a device. Please log in first.');
                return;
            }
            if (!resp.ok) throw new Error('Lookup failed');
            const data = await resp.json();

            if (data.valid) {
                currentCode = code;
                document.getElementById('info-client').textContent = data.client_name || 'Unknown';
                document.getElementById('info-scopes').textContent = data.scopes || 'all';
                deviceInfo.classList.remove('hidden');
                actionButtons.classList.remove('hidden');
                errorText.classList.add('hidden');
            } else {
                deviceInfo.classList.add('hidden');
                actionButtons.classList.add('hidden');
                showError('Code not found or expired. Please check and try again.');
            }
        } catch (_err) {
            showError('Failed to verify code. Please try again.');
        }
    }

    async function handleAction(action) {
        btnApprove.disabled = true;
        btnDeny.disabled = true;

        try {
            const resp = await fetch(`${API_BASE}/api/auth/device/verify`, {
                method: 'POST',
                credentials: 'same-origin',
                headers: Object.assign({ 'Content-Type': 'application/json' }, getCsrfHeaders()),
                body: JSON.stringify({ user_code: currentCode, action: action })
            });

            if (!resp.ok) {
                const err = await resp.json().catch(() => {
                    return {};
                });
                throw new Error(err.message || 'Action failed');
            }

            document.getElementById('step-code').classList.add('hidden');
            if (action === 'approve') {
                document.getElementById('status-success').classList.remove('hidden');
            } else {
                document.getElementById('status-denied').classList.remove('hidden');
            }
        } catch (err) {
            btnApprove.disabled = false;
            btnDeny.disabled = false;
            showError(err.message || 'Failed to process action.');
        }
    }

    function showError(msg) {
        errorText.textContent = msg;
        errorText.classList.remove('hidden');
    }
})();
