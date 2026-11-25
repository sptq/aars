// Motyw panelu: system -> jasny -> ciemny. Ładowany blokująco w <head>,
// żeby motyw był ustawiony przed pierwszym malowaniem. Bez JS panel
// po prostu podąża za ustawieniem systemu.
(function () {
  var KEY = 'aars-theme';
  var root = document.documentElement;
  root.classList.add('js');

  function read() {
    try {
      var v = localStorage.getItem(KEY);
      return v === 'light' || v === 'dark' ? v : '';
    } catch (e) {
      return '';
    }
  }
  function write(v) {
    try {
      if (v) localStorage.setItem(KEY, v);
      else localStorage.removeItem(KEY);
    } catch (e) {
      /* pamięć niedostępna: motyw działa do końca strony */
    }
  }
  function apply(v) {
    if (v) root.dataset.theme = v;
    else delete root.dataset.theme;
  }
  function label(v) {
    return 'Motyw: ' + (v === 'light' ? 'jasny' : v === 'dark' ? 'ciemny' : 'systemowy');
  }

  var current = read();
  apply(current);

  // Przycisk bywa podmieniany razem z <body> (hx-boost), więc słuchamy na
  // dokumencie i odświeżamy napis po każdej podmianie.
  function refresh() {
    var buttons = document.querySelectorAll('[data-theme-toggle]');
    for (var i = 0; i < buttons.length; i++) buttons[i].textContent = label(current);
  }
  document.addEventListener('DOMContentLoaded', refresh);
  document.addEventListener('htmx:afterSettle', refresh);
  document.addEventListener('click', function (e) {
    if (!e.target.closest || !e.target.closest('[data-theme-toggle]')) return;
    current = current === '' ? 'light' : current === 'light' ? 'dark' : '';
    apply(current);
    write(current);
    refresh();
  });
})();
