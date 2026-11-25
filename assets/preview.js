// „Odsłuchaj”: synteza tekstu przez POST /preview (formularz nadania,
// definicja komunikatu, podgląd biuletynu) albo gotowy plik zapisanego
// komunikatu. Kliknięcia łapie dokument, więc przyciski mogą przychodzić
// w podmienianych kartach (htmx). Bez JS przycisku nic nie robi.
(function () {
  'use strict';

  function csrf(btn) {
    var field = (btn.form && btn.form.elements.csrf) || document.querySelector('input[name=csrf]');
    return field ? field.value : '';
  }

  document.addEventListener('click', function (e) {
    var btn = e.target.closest && e.target.closest('[data-preview]');
    if (!btn) return;
    var src = btn.getAttribute('data-preview-src');
    if (src) {
      var player = document.getElementById('preview-audio');
      player.src = src;
      player.play();
      return;
    }
    var box = btn.closest('.preview');
    var audio = box.querySelector('audio');
    var note = box.querySelector('.preview-note');
    var source = btn.getAttribute('data-preview-text');
    var text = source ? document.getElementById(source).value : btn.form.elements.text.value;
    var body = new URLSearchParams();
    body.set('text', text);
    body.set('csrf', csrf(btn));
    note.textContent = 'Synteza…';
    fetch('/preview', { method: 'POST', body: body })
      .then(function (r) {
        if (!r.ok) return r.text().then(function (t) { throw new Error(t); });
        return r.blob();
      })
      .then(function (blob) {
        note.textContent = '';
        if (audio.dataset.url) URL.revokeObjectURL(audio.dataset.url);
        audio.dataset.url = URL.createObjectURL(blob);
        audio.src = audio.dataset.url;
        audio.play();
      })
      .catch(function (err) { note.textContent = err.message; });
  });
})();
