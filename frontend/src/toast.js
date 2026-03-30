import Toastify from 'toastify-js';

export function toast(message, type = 'success') {
  const isError = type === 'error';
  Toastify({
    text: message,
    duration: isError ? 5000 : 3000,
    gravity: 'bottom',
    position: 'center',
    stopOnFocus: true,
    className: `app-toast ${isError ? 'app-toast-error' : 'app-toast-success'}`,
  }).showToast();
}
