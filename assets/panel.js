// Panel bez przeładowań (etap 13, D): powiadomienia z nagłówka HX-Trigger,
// karta z błędem (4xx z HTML) wstawiana w miejsce starej, fokus po podmianie,
// przyciski zablokowane na czas żądania. Bez JS formularze idą zwykłym POST.
(function () {
  'use strict';
  htmx.config.historyCacheSize = 0;         // wstecz = świeża strona z serwera, nie
  htmx.config.refreshOnHistoryMiss = true;  // zrzut w localStorage (hasła tymczasowe!)
  var FOCUSABLE = 'a[href], button, input:not([type=hidden]), select, textarea';
  var submitter = null, restore = null, slice = Array.prototype.slice;
  var on = function (name, fn, capture) { document.addEventListener(name, fn, capture); };

  function toast(kind, text) {
    var box = document.getElementById('toasts'), el = document.createElement('div');
    if (!box || !text) return;
    el.className = 'toast toast-' + (kind === 'error' ? 'error' : 'ok');
    el.textContent = text;
    el.addEventListener('click', function () { el.remove(); });
    box.appendChild(el);
    setTimeout(function () { el.remove(); }, kind === 'error' ? 8000 : 3000);
  }
  function buttons(elt) {
    return elt.tagName === 'BUTTON' ? [elt] : elt.tagName === 'FORM' ? slice.call(elt.querySelectorAll('button')) : [];
  }
  on('toast', function (e) { toast(e.detail.kind, e.detail.text); });
  on('submit', function (e) { submitter = e.submitter || null; }, true);
  on('htmx:sendError', function () { toast('error', 'Brak połączenia ze stacją — nic nie zapisano.'); });
  on('htmx:beforeRequest', function (e) {
    var elt = e.detail.elt, busy = elt.tagName === 'BUTTON' ? elt : submitter;
    buttons(elt).forEach(function (b) { if (!b.disabled) { b.disabled = true; b.dataset.busy = '1'; } });
    if (busy && elt.contains(busy)) busy.classList.add('is-busy');
  });
  on('htmx:afterRequest', function (e) {
    buttons(e.detail.elt).forEach(function (b) {
      if (b.dataset.busy) { b.disabled = false; delete b.dataset.busy; }
      b.classList.remove('is-busy');
    });
  });
  on('htmx:beforeSwap', function (e) {
    var d = e.detail, xhr = d.xhr, t = d.target;
    var html = /text\/html/.test(xhr.getResponseHeader('Content-Type') || '');
    if (xhr.status >= 400) {        // karta z błędem przy polu; sam tekst → powiadomienie
      d.shouldSwap = html;
      d.isError = !html;
      if (!html && !xhr.getResponseHeader('HX-Trigger')) toast('error', xhr.responseText || 'Błąd ' + xhr.status);
    } else if (d.boosted && html && !/<body[^>]*data-panel(?![^>]*data-full-load)/.test(xhr.responseText)) {
      d.shouldSwap = false;         // logowanie albo strona z mapą: zwykłe wejście
      window.location.assign(xhr.responseURL || d.pathInfo.finalRequestPath);
    }
    var at = submitter && t.contains(submitter) ? submitter : document.activeElement;
    restore = d.shouldSwap && t.id && !d.boosted
      ? { id: t.id, error: xhr.status >= 400, at: slice.call(t.querySelectorAll(FOCUSABLE)).indexOf(at) } : null;
  });
  on('htmx:afterSettle', function () {   // fokus: pierwsze błędne pole albo ten sam przycisk
    var r = restore, card = r && document.getElementById(r.id), el;
    restore = null;
    if (!card) return;
    el = r.error && (card.querySelector('[aria-invalid="true"]') || card.querySelector('.error'));
    if (el && !el.matches(FOCUSABLE)) el.setAttribute('tabindex', '-1');
    el = el || card.querySelectorAll(FOCUSABLE)[r.at];
    if (el) el.focus({ preventScroll: true });
  });
})();
