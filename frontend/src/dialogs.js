import { Sound } from './sound.js';
import { esc } from './utils.js';

// showConfirm renders an in-app confirmation dialog. Returns a promise that resolves true/false.
export function showConfirm(message, options = {}) {
  const { confirmText = 'Confirm', cancelText = 'Cancel', destructive = false } = options;
  return new Promise(resolve => {
    document.getElementById('dialog-overlay')?.remove();
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay';
    overlay.id = 'dialog-overlay';
    overlay.innerHTML = `
      <div class="modal" style="width:380px">
        <div style="padding:20px 18px 8px">
          <p style="margin:0;font-family:var(--font-sans);font-size:13px;color:var(--text);line-height:1.5;white-space:pre-line">${esc(message)}</p>
        </div>
        <div style="display:flex;justify-content:flex-end;gap:8px;padding:12px 18px 16px">
          <button class="btn-secondary" id="dialog-cancel">${esc(cancelText)}</button>
          <button class="${destructive ? 'btn-danger' : 'btn-primary'}" id="dialog-confirm">${esc(confirmText)}</button>
        </div>
      </div>
    `;
    document.body.appendChild(overlay);
    Sound.ui('soft');

    const close = (result) => { overlay.remove(); resolve(result); };
    overlay.querySelector('#dialog-cancel').addEventListener('click', () => close(false));
    overlay.querySelector('#dialog-confirm').addEventListener('click', () => close(true));
    overlay.addEventListener('click', (e) => { if (e.target === overlay) close(false); });
    overlay.querySelector('#dialog-confirm').focus();
  });
}

// showPrompt renders an in-app input dialog. Returns a promise that resolves to the string or null.
export function showPrompt(message, defaultValue = '', options = {}) {
  const { confirmText = 'OK', cancelText = 'Cancel', validate } = options;
  return new Promise(resolve => {
    document.getElementById('dialog-overlay')?.remove();
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay';
    overlay.id = 'dialog-overlay';
    overlay.innerHTML = `
      <div class="modal" style="width:380px">
        <div style="padding:20px 18px 8px;display:flex;flex-direction:column;gap:10px">
          <p style="margin:0;font-family:var(--font-sans);font-size:13px;color:var(--text);line-height:1.5">${esc(message)}</p>
          <input type="text" id="dialog-input" class="field-input" value="${esc(defaultValue)}" spellcheck="false" autocomplete="off" style="width:100%;box-sizing:border-box">
          <div id="dialog-input-error" style="font-size:11px;color:var(--red);display:none"></div>
        </div>
        <div style="display:flex;justify-content:flex-end;gap:8px;padding:12px 18px 16px">
          <button class="btn-secondary" id="dialog-cancel">${esc(cancelText)}</button>
          <button class="btn-primary" id="dialog-confirm">${esc(confirmText)}</button>
        </div>
      </div>
    `;
    document.body.appendChild(overlay);
    Sound.ui('soft');

    const input = overlay.querySelector('#dialog-input');
    const errorEl = overlay.querySelector('#dialog-input-error');
    input.focus();
    input.select();

    const close = (result) => { overlay.remove(); resolve(result); };

    function tryConfirm() {
      const val = input.value.trim();
      if (!val) { input.focus(); return; }
      if (validate) {
        const err = validate(val);
        if (err) {
          errorEl.textContent = err;
          errorEl.style.display = 'block';
          input.focus();
          return;
        }
      }
      close(val);
    }

    input.addEventListener('input', () => { errorEl.style.display = 'none'; });
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter') tryConfirm(); });
    overlay.querySelector('#dialog-cancel').addEventListener('click', () => close(null));
    overlay.querySelector('#dialog-confirm').addEventListener('click', tryConfirm);
    overlay.addEventListener('click', (e) => { if (e.target === overlay) close(null); });
  });
}
