// Panel bez przeładowań (etap 13, D): powiadomienia z nagłówka HX-Trigger,
// karta z błędem (4xx z HTML) wstawiana w miejsce starej, fokus po podmianie,
// przyciski zablokowane na czas żądania. Bez JS formularze idą zwykłym POST.
(function () {
  'use strict';
  // Bez zrzutów stron w localStorage (hasła tymczasowe, stan nadawania):
  // wstecz/naprzód pobiera stronę od nowa przez htmx (HX-History-Restore-Request).
  htmx.config.historyCacheSize = 0;
  htmx.config.refreshOnHistoryMiss = false;
  htmx.config.timeout = 15000;  // dłuższe żądania mają hx-request='{"timeout":…}'
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
  // Pełna strona spoza panelu (logowanie) albo z własnymi skryptami (mapa)
  // nie nadaje się do podmiany — wchodzimy na nią zwykłym ładowaniem.
  function needsFullLoad(text) {
    var body = /<body[^>]*>/.exec(text);
    return !!body && (body[0].indexOf('data-panel') < 0 || body[0].indexOf('data-full-load') >= 0);
  }
  function buttons(elt) {
    return elt.tagName === 'BUTTON' ? [elt] : elt.tagName === 'FORM' ? slice.call(elt.querySelectorAll('button')) : [];
  }
  // Fokus po podmianie wraca do „tego samego” elementu: po id, a bez id po
  // formularzu (action / hx-post — w wierszu tabeli jest unikalny) i pozycji w nim.
  function locate(card, el) {
    if (!el || !card.contains(el)) return null;
    if (el.id) return { sel: '#' + CSS.escape(el.id), i: -1 };
    var owner = el.closest('form[action], [hx-post]');
    if (!owner || !card.contains(owner)) return null;
    var attr = owner.tagName === 'FORM' ? 'action' : 'hx-post';
    return { sel: owner.tagName.toLowerCase() + '[' + attr + '="' + CSS.escape(owner.getAttribute(attr)) + '"]',
             i: owner === el ? -1 : slice.call(owner.querySelectorAll(FOCUSABLE)).indexOf(el) };
  }
  function find(card, loc) {
    var owner = loc && card.querySelector(loc.sel);
    return owner && loc.i >= 0 ? owner.querySelectorAll(FOCUSABLE)[loc.i] : owner;
  }

  on('toast', function (e) { toast(e.detail.kind, e.detail.text); });
  on('submit', function (e) { submitter = e.submitter || null; }, true);
  on('htmx:sendError', function () { toast('error', 'Brak połączenia ze stacją — nic nie zapisano.'); });
  on('htmx:timeout', function () { toast('error', 'Stacja nie odpowiada — sprawdź wynik i spróbuj ponownie.'); });
  on('htmx:beforeRequest', function (e) {
    var elt = e.detail.elt, busy = elt.tagName === 'BUTTON' ? elt : submitter;
    buttons(elt).forEach(function (b) { if (!b.disabled) { b.disabled = true; b.dataset.busy = '1'; } });
    if (busy && elt.contains(busy)) busy.classList.add('is-busy');
  });
  on('htmx:afterRequest', function (e) {  // także po błędzie sieci i przekroczeniu czasu
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
    } else if (html && needsFullLoad(xhr.responseText)) {
      d.shouldSwap = false;         // np. sesja wygasła w trakcie: strona logowania
      window.location.assign(xhr.responseURL || d.pathInfo.finalRequestPath);
    }
    var at = submitter && t.contains(submitter) ? submitter : document.activeElement;
    var link = d.requestConfig && d.requestConfig.elt && d.requestConfig.elt.tagName === 'A';
    restore = d.shouldSwap && t.id && !d.boosted ? { id: t.id, error: xhr.status >= 400, at: locate(t, at), link: link } : null;
  });
  on('htmx:afterSettle', function () {   // fokus: pierwsze błędne pole albo ten sam element
    var r = restore, card = r && document.getElementById(r.id), el;
    restore = null;
    if (!card) return;
    el = r.error && (card.querySelector('[aria-invalid="true"]') || card.querySelector('.error'));
    if (el && !el.matches(FOCUSABLE)) el.setAttribute('tabindex', '-1');
    el = el || find(card, r.at);
    if (el) el.focus({ preventScroll: true });
    // Lista nad szczegółami (wąski ekran, Moduły): po wyborze z listy pokaż szczegóły.
    var show = r.link && card.dataset.scrollMobile && document.querySelector(card.dataset.scrollMobile);
    if (show && window.matchMedia('(max-width: 899px)').matches) show.scrollIntoView({ block: 'start' });
  });
  on('htmx:historyCacheMissLoad', function (e) {
    if (needsFullLoad(e.detail.xhr.responseText)) window.location.reload();
  });
})();
