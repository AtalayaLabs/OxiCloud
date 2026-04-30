// Read error type from URL query parameter
var params = new URLSearchParams(window.location.search);
var errorType = params.get('type') || 'generic';

var errorTitle = document.getElementById('error-title');
var errorMessage = document.getElementById('error-message');
var errorAction = document.getElementById('error-action');

if (!errorTitle || !errorMessage || !errorAction) {
    throw new Error('missing html elements');
}

switch (errorType) {
    case 'invalid-credentials':
        errorTitle.textContent = 'Login Failed';
        errorMessage.textContent = 'Invalid username or password. Please check your credentials and try again.';
        errorAction.textContent = 'Try Again';
        errorAction.addEventListener('click', () => {
            history.back();
        });
        break;
    case 'session-expired':
        errorTitle.textContent = 'Session Expired';
        errorMessage.textContent = 'Your session has expired. Please try again.';
        errorAction.textContent = 'Close Window';
        errorAction.addEventListener('click', () => {
            window.close();
        });
        break;
    case 'not-found':
        errorTitle.textContent = 'Not Found';
        errorMessage.textContent = 'The requested page was not found.';
        errorAction.textContent = 'Close Window';
        errorAction.addEventListener('click', () => {
            window.close();
        });
        break;
    default:
        errorTitle.textContent = 'Error';
        errorMessage.textContent = 'An unexpected error occurred. Please try again.';
        errorAction.textContent = 'Close Window';
        errorAction.addEventListener('click', () => {
            window.close();
        });
}
