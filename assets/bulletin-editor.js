// Edytor biuletynu z klocków (etap 13, 4a; strona /bulletin).
//
// Serwer renderuje układ do odczytu i formularz z polem `layout` (JSON).
// Ten skrypt zamienia listę w karty z <template id="blk-template">: uchwyt
// do przeciągania (HTML5 drag and drop) i strzałki W górę/W dół (klawiatura),
// przełącznik, pauzy w sekundach, „kiedy”, tekst ze zmiennymi. Stan żyje
// w tablicy `blocks`, a każda zmiana zapisuje go w polu `layout` — zapis
// idzie zwykłym formularzem (htmx), więc walidacja jest jedna, po stronie
// serwera. Oś czasu liczy czas jak kompozytor: 10 znaków ≈ 1 s, pauza
// między klockami = po poprzednim + przed następnym (najwyżej 10 s).
// Operator (bez prawa zmian) dostaje samą oś czasu. Bez JS nic tu nie działa,
// a strona pokazuje układ do odczytu i pole JSON.
(function () {
  'use strict';
  var MAX_BLOCKS = 24, MAX_PAUSE_MS = 10000, MAX_TEXT = 500, VARS = ['znak', 'dzien', 'data', 'godzina'];
  var DAYS = ['niedziela', 'poniedziałek', 'wtorek', 'środa', 'czwartek', 'piątek', 'sobota'];
  var MONTHS = ['stycznia', 'lutego', 'marca', 'kwietnia', 'maja', 'czerwca', 'lipca', 'sierpnia',
    'września', 'października', 'listopada', 'grudnia'];
  var slice = Array.prototype.slice;
  var ed = null;   // bieżący edytor (strona ma najwyżej jeden)
  // Czas stacji (Warszawa) zamiast strefy przeglądarki: serwer podaje
  // „teraz” w Warszawie, a my przesuwamy zegar o różnicę. Wynik czytamy
  // getHours()/getDate() jako czas warszawski.
  var clockShift = 0;
  function stationNow() { return new Date(Date.now() + clockShift); }
  function setClock(text) {
    var m = /^(\d+)-(\d+)-(\d+)T(\d+):(\d+):(\d+)$/.exec(text || '');
    if (m) clockShift = new Date(+m[1], m[2] - 1, +m[3], +m[4], +m[5], +m[6]).getTime() - Date.now();
  }
  // Układ, z którego pochodzi karta podglądu (JSON), i klocki w chwili żądania:
  // długości z podglądu przypisujemy po tożsamości klocka, nie po pozycji.
  var previewFor = null;

  function all(root, sel) { return slice.call(root.querySelectorAll(sel)); }
  function pad(n) { return (n < 10 ? '0' : '') + n; }
  function secs(ms) { return (Math.round(ms / 100) / 10).toString().replace('.', ','); }
  function reduced() { return window.matchMedia && matchMedia('(prefers-reduced-motion: reduce)').matches; }

  // ---- model ---------------------------------------------------------------

  var uidSeq = 0;
  function fromJson(o) {
    var k = o.kiedy || { typ: 'zawsze' };
    return {
      uid: ++uidSeq, kind: o.rodzaj, enabled: o.wlaczony !== false,
      before: o.pauza_przed_ms || 0, after: o.pauza_po_ms || 0,
      when: k.typ || 'zawsze', n: k.n || 3, od: k.od || '22:00', do: k['do'] || '06:00',
      text: o.tekst || '', say: !!o.mow_gdy_brak, touched: true
    };
  }
  function toJson(b, info) {
    var kiedy = b.when === 'co_n_cykli' ? { typ: b.when, n: b.n }
      : b.when === 'w_godzinach' ? { typ: b.when, od: b.od, 'do': b['do'] } : { typ: 'zawsze' };
    return {
      rodzaj: b.kind, wlaczony: b.enabled, pauza_przed_ms: b.before, pauza_po_ms: b.after, kiedy: kiedy,
      tekst: info && info.text ? b.text : '', mow_gdy_brak: b.kind === 'ostrzezenia' && b.say
    };
  }
  function parseLayout(text) {
    var o = JSON.parse(text);
    if (!o || !Array.isArray(o.bloki)) throw new Error('bez listy bloków');
    return o.bloki.map(fromJson);
  }

  // Szablon na teraz, jak `render_template` na serwerze: bez znaku stacji
  // zdania z {znak} wypadają, zdanie od zmiennej dostaje wielką literę.
  function renderTemplate(text, callsign) {
    var now = stationNow();
    var values = {
      znak: callsign, dzien: DAYS[now.getDay()], data: now.getDate() + ' ' + MONTHS[now.getMonth()],
      godzina: now.getHours() + ':' + pad(now.getMinutes())
    };
    var sentences = text.match(/[^.!?]+(?:[.!?]+(?=\s|$)|$)/g) || [];
    return sentences.map(function (s) { return s.trim(); })
      .filter(function (s) { return s && (callsign || s.indexOf('{znak}') < 0); })
      .map(function (s) {
        var out = s.replace(/\{(znak|dzien|data|godzina)\}/g, function (_, v) { return values[v]; });
        return out.charAt(0).toUpperCase() + out.slice(1);
      }).join(' ');
  }

  // Błąd klocka po polsku albo ''. To samo sprawdza serwer (`Layout::validate`).
  function blockError(b, info, i, blocks) {
    if (info && !info.text && blocks.slice(0, i).some(function (o) { return o.kind === b.kind; }))
      return 'Ten klocek już jest w układzie.';
    if (!(b.before >= 0 && b.before <= MAX_PAUSE_MS) || !(b.after >= 0 && b.after <= MAX_PAUSE_MS))
      return 'Pauza od 0 do 10 s.';
    if (b.when === 'co_n_cykli' && !(b.n >= 2 && b.n <= 24 && b.n % 1 === 0)) return 'Co N-ty cykl: N od 2 do 24.';
    if (b.when === 'w_godzinach') {
      if (!/^\d\d:\d\d$/.test(b.od) || !/^\d\d:\d\d$/.test(b['do'])) return 'Podaj godziny od i do.';
      if (b.od === b['do']) return 'Początek i koniec godzin muszą się różnić.';
    }
    if (info && info.text) {
      var t = b.text.trim();
      if (!t) return b.touched ? 'Wpisz tekst.' : '';
      if (t.length > MAX_TEXT) return 'Tekst najwyżej ' + MAX_TEXT + ' znaków.';
      if (t.indexOf('[[pauza') >= 0) return 'Pauzy ustawia się polami pauzy przed i po klocku.';
      var depth = 0, bad = '';
      t.replace(/[{}]/g, function (c) { depth += c === '{' ? 1 : -1; if (depth < 0 || depth > 1) bad = 'Niedomknięty nawias w tekście.'; return c; });
      if (bad || depth !== 0) return 'Niedomknięty nawias „{” w tekście.';
      var unknown = (t.match(/\{([^}]*)\}/g) || []).filter(function (m) { return VARS.indexOf(m.slice(1, -1)) < 0; });
      if (unknown.length) return 'Nieznana zmienna ' + unknown[0] + ' (dozwolone: {znak}, {dzien}, {data}, {godzina}).';
    }
    return '';
  }

  // ---- edytor --------------------------------------------------------------

  function Editor(form) {
    var self = this;
    this.form = form;
    this.catalog = {};
    JSON.parse(form.dataset.catalog || '[]').forEach(function (k) { self.catalog[k.id] = k; });
    this.order = Object.keys(this.catalog);
    this.callsign = form.dataset.callsign || '';
    this.limitS = +form.dataset.limitS || 0;
    this.editable = form.dataset.editable === '1';
    this.field = form.querySelector('textarea[name=layout]');
    this.list = document.getElementById('bulletin-blocks');
    this.tl = document.getElementById('bulletin-timeline');
    this.tpl = document.getElementById('blk-template');
    this.live = document.getElementById('bulletin-announce');
    this.history = [];
    this.previewChars = {};    // uid → znaki z ostatniego podglądu
    this.previewUids = null;
    setClock(form.dataset.now);
    this.blocks = parseLayout(this.editable ? this.field.value : form.dataset.layout);
    all(document, '#bulletin-editor [data-js-show]').forEach(function (el) { el.hidden = false; });
    if (this.editable) this.startEditing();
    this.readPreview();
    this.timeline();
  }

  Editor.prototype.startEditing = function () {
    var self = this, form = this.form;
    // Błąd serwera przy klocku zostaje przy nim, dopóki klocek się nie zmieni.
    var serverErrors = {};
    all(this.list, '.blk-static.is-invalid').forEach(function (li) {
      var e = li.querySelector('.error');
      serverErrors[+li.id.replace('blk-', '') - 1] = e ? e.textContent : '';
    });
    this.blocks.forEach(function (b, i) { if (serverErrors[i] != null) b.serverError = serverErrors[i]; });
    all(form, '[data-nojs]').forEach(function (el) { el.hidden = true; });
    var hint = document.querySelector('[data-out=preview-hint]');
    if (hint) hint.textContent = 'Składa biuletyn na teraz z układu, który widzisz (także niezapisanego); nic nie nadaje i nic nie zapisuje.';
    this.initial = this.snapshot();
    this.serverJson = this.json();
    this.dirtyFromServer = form.dataset.dirty === '1';
    this.render();
    this.modeUi();
    this.bind();
    this.sync();
    var bad = this.list.querySelector('.is-invalid');
    if (bad) {
      bad.scrollIntoView({ block: 'center', behavior: reduced() ? 'auto' : 'smooth' });
      var f = bad.querySelector('[aria-invalid="true"]') || bad.querySelector('textarea, input');
      if (f) f.focus({ preventScroll: true });
    }
  };

  Editor.prototype.info = function (b) { return this.catalog[b.kind] || { label: b.kind, desc: '' }; };

  Editor.prototype.json = function () {
    var self = this;
    return JSON.stringify({ bloki: this.blocks.map(function (b) { return toJson(b, self.info(b)); }) }, null, 2);
  };

  Editor.prototype.modeValues = function () {
    var f = this.form.elements, mode = this.form.querySelector('input[name=mode]:checked');
    return [mode ? mode.value : '', f.interval_min ? f.interval_min.value : '', f.times ? f.times.value : ''].join('|');
  };
  Editor.prototype.snapshot = function () { return this.json() + '\n' + this.modeValues(); };
  Editor.prototype.isDirtyLayout = function () { return !!this.dirtyFromServer || (this.initial != null && this.snapshot() !== this.initial); };
  Editor.prototype.isDirty = function () { return this.dirtyFromServer || this.snapshot() !== this.initial; };

  Editor.prototype.remember = function () {
    var copy = this.blocks.map(function (b) { var o = {}; for (var k in b) o[k] = b[k]; return o; });
    this.history.push(copy);
    if (this.history.length > 50) this.history.shift();
  };

  Editor.prototype.announce = function (text) {
    if (!this.live) return;
    this.live.textContent = '';
    var live = this.live;
    setTimeout(function () { live.textContent = text; }, 30);
  };

  // Karty od nowa (po zmianie kolejności, dodaniu, usunięciu, cofnięciu).
  // FLIP: karty płynnie przesuwają się na nowe miejsca.
  Editor.prototype.render = function (focus) {
    var self = this, before = {};
    all(this.list, '.blk-edit').forEach(function (li) { before[li.dataset.uid] = li.getBoundingClientRect().top; });
    this.list.textContent = '';
    this.blocks.forEach(function (b, i) { self.list.appendChild(self.card(b, i)); });
    if (!reduced()) {
      all(this.list, '.blk-edit').forEach(function (li) {
        var was = before[li.dataset.uid];
        if (was == null) { li.classList.add('is-new'); return; }
        var dy = was - li.getBoundingClientRect().top;
        if (!dy) return;
        li.style.transform = 'translateY(' + dy + 'px)';
        li.style.transition = 'none';
        requestAnimationFrame(function () {
          li.style.transition = 'transform 180ms ease';
          li.style.transform = '';
        });
      });
    }
    if (focus) {
      var li = this.list.querySelector('[data-uid="' + focus.uid + '"]');
      var el = li && (li.querySelector(focus.sel) || li.querySelector('[data-act=grip]'));
      if (el && el.disabled) el = li.querySelector('[data-act=' + (focus.sel.indexOf('up') >= 0 ? 'down' : 'up') + ']');
      if (el) el.focus({ preventScroll: false });
    }
    this.addUi();
  };

  Editor.prototype.card = function (b, i) {
    var info = this.info(b), li = this.tpl.content.firstElementChild.cloneNode(true), n = this.blocks.length;
    li.dataset.uid = b.uid;
    li.id = 'blk-' + (i + 1);
    li.classList.add('k-' + b.kind);
    li.querySelector('.blk-num').textContent = i + 1;
    li.querySelector('.blk-name').textContent = info.label;
    li.querySelector('.blk-desc').textContent = info.desc;
    var q = function (sel) { return li.querySelector(sel); };
    q('[data-f=enabled]').checked = b.enabled;
    q('[data-f=enabled]').setAttribute('aria-label', 'Nadawany: ' + info.label);
    q('[data-act=grip]').setAttribute('aria-label', 'Przenieś „' + info.label + '” (pozycja ' + (i + 1) + ' z ' + n + '): strzałki w górę i w dół');
    q('[data-act=up]').setAttribute('aria-label', 'Przesuń „' + info.label + '” w górę');
    q('[data-act=down]').setAttribute('aria-label', 'Przesuń „' + info.label + '” w dół');
    q('[data-act=up]').disabled = i === 0;
    q('[data-act=down]').disabled = i === n - 1;
    q('[data-f=before]').value = secs(b.before).replace(',', '.');
    q('[data-f=after]').value = secs(b.after).replace(',', '.');
    q('[data-f=when]').value = b.when;
    q('[data-f=n]').value = b.n;
    q('[data-f=od]').value = b.od;
    q('[data-f=do]').value = b['do'];
    q('[data-f=text]').value = b.text;
    q('[data-f=say_when_empty]').checked = b.say;
    q('[data-part=text]').hidden = !info.text;
    q('[data-part=warn]').hidden = !info.warn;
    q('[data-part=remove]').hidden = b.kind !== 'tekst';
    if (b.kind === 'tekst') q('[data-f=text]').placeholder = 'Np. Zapraszamy na niedzielną sieć o 20:00 na przemienniku.';
    this.refreshCard(li, b, i);
    return li;
  };

  // Części karty zależne od wartości (bez przebudowy — fokus i kursor zostają).
  Editor.prototype.refreshCard = function (li, b, i) {
    var info = this.info(b), q = function (sel) { return li.querySelector(sel); };
    li.classList.toggle('is-off', !b.enabled);
    all(li, '[data-part=n]').forEach(function (el) { el.hidden = b.when !== 'co_n_cykli'; });
    all(li, '[data-part=hours]').forEach(function (el) { el.hidden = b.when !== 'w_godzinach'; });
    if (info.text) q('[data-out=hear]').textContent = renderTemplate(b.text, this.callsign) || '—';
    q('[data-out=when]').textContent = this.whenHint(b);
    q('[data-out=when]').hidden = !q('[data-out=when]').textContent;
    var err = b.serverError || blockError(b, info, i, this.blocks);
    var box = q('[data-out=error]');
    box.hidden = !err;
    box.textContent = err;
    box.id = 'blk-' + (i + 1) + '-error';
    li.classList.toggle('is-invalid', !!err);
    var field = err ? this.fieldFor(li, b, err) : null;
    all(li, '[data-f]').forEach(function (el) {
      if (el === field) { el.setAttribute('aria-invalid', 'true'); el.setAttribute('aria-describedby', box.id); }
      else { el.removeAttribute('aria-invalid'); el.removeAttribute('aria-describedby'); }
    });
  };

  Editor.prototype.fieldFor = function (li, b, err) {
    var q = function (sel) { return li.querySelector(sel); };
    if (/Pauza/.test(err)) return (b.before >= 0 && b.before <= MAX_PAUSE_MS) ? q('[data-f=after]') : q('[data-f=before]');
    if (/N od/.test(err)) return q('[data-f=n]');
    if (/godzin/.test(err)) return q('[data-f=od]');
    if (this.info(b).text) return q('[data-f=text]');
    return q('[data-f=enabled]');
  };

  Editor.prototype.cycleMinutes = function () {
    var mode = this.form.querySelector('input[name=mode]:checked');
    if (mode && mode.value !== 'cykl') return 0;
    var n = parseInt(this.form.elements.interval_min && this.form.elements.interval_min.value, 10);
    return n >= 5 && n <= 60 ? n : 10;
  };

  Editor.prototype.whenHint = function (b) {
    if (b.when === 'zawsze') return '';
    if (b.when === 'co_n_cykli') {
      var m = this.cycleMinutes();
      if (!m) return 'W trybie stałych pór „co N-ty cykl” nie ogranicza — klocek idzie za każdym razem.';
      var every = m * b.n;
      return 'W co ' + b.n + '. biuletynie — mniej więcej co ' + (every >= 60 && every % 60 === 0 ? every / 60 + ' h' : every + ' min') + '.';
    }
    var night = b.od > b['do'];
    return 'Tylko między ' + b.od + ' a ' + b['do'] + (night ? ' (przez północ)' : '') + ', czas polski.';
  };

  // Tryb: pola interwału i pór tylko dla wybranego trybu; podpowiedź pór cyklu.
  Editor.prototype.modeUi = function () {
    var mode = this.form.querySelector('input[name=mode]:checked');
    var value = mode ? mode.value : 'cykl';
    all(this.form, '[data-mode]').forEach(function (el) { el.hidden = el.dataset.mode !== value; });
    var hint = this.form.querySelector('[data-out=cycle-hint]'), m = this.cycleMinutes();
    if (hint && m) {
      var now = stationNow(), minutes = now.getHours() * 60 + now.getMinutes(), next = [];
      for (var t = 0; t < 1440 && next.length < 4; t += m) if (t > minutes) next.push(pad(Math.floor(t / 60)) + ':' + pad(t % 60));
      hint.textContent = 'Wyrównane do północy. Najbliższe: ' + (next.length ? next.join(', ') + '…' : 'jutro 00:00') +
        (1440 % m ? ' (doba nie dzieli się równo — o północy cykl zaczyna się od nowa).' : '');
    }
  };

  Editor.prototype.addUi = function () {
    var self = this, select = this.form.querySelector('[data-f=add-kind]');
    if (!select) return;
    var present = this.blocks.map(function (b) { return b.kind; });
    var missing = this.order.filter(function (k) { return k !== 'tekst' && present.indexOf(k) < 0; });
    select.textContent = '';
    missing.forEach(function (k) {
      var o = document.createElement('option');
      o.value = k;
      o.textContent = self.catalog[k].label;
      select.appendChild(o);
    });
    var full = this.blocks.length >= MAX_BLOCKS;
    select.closest('label').hidden = !missing.length;
    this.form.querySelector('[data-act=add]').hidden = !missing.length;
    this.form.querySelector('[data-act=add]').disabled = full;
    this.form.querySelector('[data-act=add-text]').disabled = full;
    var count = this.form.querySelector('[data-out=count]');
    var on = this.blocks.filter(function (b) { return b.enabled; }).length;
    if (count) count.textContent = this.blocks.length + ' klocków, nadawanych ' + on;
  };

  // Pole `layout`, pasek zapisu, przycisk nadania, oś czasu.
  Editor.prototype.sync = function () {
    var json = this.json(), self = this;
    // Błąd z serwera dotyczył wysłanego układu: po każdej zmianie układu
    // zostają tylko błędy liczone tutaj (te same reguły co na serwerze).
    if (this.serverJson != null && json !== this.serverJson) {
      this.serverJson = null;
      this.blocks.forEach(function (b, i) {
        if (b.serverError == null) return;
        delete b.serverError;
        var li = self.list.querySelector('[data-uid="' + b.uid + '"]');
        if (li) self.refreshCard(li, b, i);
      });
    }
    this.field.value = json;
    var dirty = this.isDirty(), bar = document.getElementById('bulletin-savebar');
    if (bar) {
      bar.classList.toggle('is-dirty', dirty);
      bar.querySelector('[data-out=state]').textContent = dirty ? 'Niezapisane zmiany' : 'Zapisane';
      bar.querySelector('[data-act=discard]').disabled = !dirty;
      bar.querySelector('[data-act=undo]').disabled = !this.history.length;
    }
    var send = document.getElementById('bulletin-send-btn'), hint = document.querySelector('[data-out=send-hint]');
    if (send) {
      send.disabled = dirty;
      if (hint) hint.textContent = dirty
        ? 'Najpierw zapisz albo odrzuć zmiany — „Nadaj teraz” nadaje zapisany układ, a ten, który widzisz, nie jest jeszcze zapisany.'
        : 'Składa biuletyn z zapisanego układu i świeżych danych i dodaje go do kolejki; pójdzie na antenę, gdy nadawanie jest włączone.';
    }
    this.timeline();
  };

  Editor.prototype.move = function (from, to, focusSel) {
    if (to < 0 || to >= this.blocks.length || from === to) return;
    this.remember();
    var b = this.blocks.splice(from, 1)[0];
    this.blocks.splice(to, 0, b);
    this.render({ uid: b.uid, sel: focusSel || '[data-act=grip]' });
    this.sync();
    this.announce('„' + this.info(b).label + '” na pozycji ' + (to + 1) + ' z ' + this.blocks.length + '.');
  };

  Editor.prototype.insert = function (kind) {
    if (this.blocks.length >= MAX_BLOCKS) return;
    this.remember();
    var b = fromJson({ rodzaj: kind });
    b.touched = kind !== 'tekst';
    var end = this.blocks.map(function (x) { return x.kind; }).lastIndexOf('zakonczenie');
    var at = end >= 0 ? end : this.blocks.length;
    this.blocks.splice(at, 0, b);
    this.render({ uid: b.uid, sel: kind === 'tekst' ? '[data-f=text]' : '[data-act=grip]' });
    this.sync();
    this.announce('Dodano „' + this.info(b).label + '” na pozycji ' + (at + 1) + '.');
  };

  Editor.prototype.bind = function () {
    var self = this, list = this.list, form = this.form;
    function at(el) {
      var li = el.closest('.blk-edit');
      if (!li) return null;
      for (var i = 0; i < self.blocks.length; i++) if (String(self.blocks[i].uid) === li.dataset.uid) return { i: i, li: li, b: self.blocks[i] };
      return null;
    }
    function read(el, b) {
      var f = el.dataset.f, v = el.value;
      if (f === 'enabled') b.enabled = el.checked;
      else if (f === 'say_when_empty') b.say = el.checked;
      else if (f === 'before' || f === 'after') b[f] = v.trim() === '' ? 0 : Math.round(parseFloat(v.replace(',', '.')) * 1000);
      else if (f === 'when') b.when = v;
      else if (f === 'n') b.n = parseInt(v, 10);
      else if (f === 'od' || f === 'do') b[f] = v;
      else if (f === 'text') { b.text = v; b.touched = true; }
      delete b.serverError;
    }
    var pending = null;   // stan sprzed edycji pola: jeden krok cofania na pole
    list.addEventListener('focusin', function (e) {
      if (e.target.dataset && e.target.dataset.f && at(e.target)) {
        pending = self.blocks.map(function (b) { var o = {}; for (var k in b) o[k] = b[k]; return o; });
      }
    });
    list.addEventListener('input', function (e) {
      var hit = e.target.dataset && e.target.dataset.f && at(e.target);
      if (!hit) return;
      if (pending) { self.history.push(pending); pending = null; }
      read(e.target, hit.b);
      self.refreshCard(hit.li, hit.b, hit.i);
      self.sync();
    });
    list.addEventListener('change', function (e) {
      var hit = e.target.dataset && e.target.dataset.f && at(e.target);
      if (!hit) return;
      read(e.target, hit.b);
      if (e.target.dataset.f === 'enabled') self.announce('„' + self.info(hit.b).label + '” ' + (hit.b.enabled ? 'włączony' : 'wyłączony') + '.');
      self.blocks.forEach(function (b, i) {   // duplikaty i podpowiedzi zależą od innych klocków
        var li = list.querySelector('[data-uid="' + b.uid + '"]');
        if (li) self.refreshCard(li, b, i);
      });
      self.sync();
    });
    list.addEventListener('click', function (e) {
      var btn = e.target.closest('button');
      var hit = btn && at(btn);
      if (!hit) return;
      if (btn.dataset['var']) {
        var area = hit.li.querySelector('[data-f=text]'), token = '{' + btn.dataset['var'] + '}';
        var s = area.selectionStart != null ? area.selectionStart : area.value.length, end = area.selectionEnd != null ? area.selectionEnd : s;
        self.remember();
        area.value = area.value.slice(0, s) + token + area.value.slice(end);
        area.focus();
        area.setSelectionRange(s + token.length, s + token.length);
        read(area, hit.b);
        self.refreshCard(hit.li, hit.b, hit.i);
        self.sync();
        return;
      }
      var act = btn.dataset.act;
      if (act === 'up') self.move(hit.i, hit.i - 1, '[data-act=up]');
      else if (act === 'down') self.move(hit.i, hit.i + 1, '[data-act=down]');
      else if (act === 'remove') {
        self.remember();
        self.blocks.splice(hit.i, 1);
        var next = self.blocks[Math.min(hit.i, self.blocks.length - 1)];
        self.render(next ? { uid: next.uid, sel: '[data-act=grip]' } : null);
        self.sync();
        self.announce('Usunięto „' + self.info(hit.b).label + '”. „Cofnij” przywraca klocek.');
      }
    });
    list.addEventListener('keydown', function (e) {
      if (!e.target.matches('[data-act=grip]')) return;
      var hit = at(e.target);
      if (!hit) return;
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
        e.preventDefault();
        self.move(hit.i, hit.i + (e.key === 'ArrowUp' ? -1 : 1));
      } else if (e.key === 'Home' || e.key === 'End') {
        e.preventDefault();
        self.move(hit.i, e.key === 'Home' ? 0 : self.blocks.length - 1);
      }
    });
    this.bindDrag();

    form.addEventListener('click', function (e) {
      var btn = e.target.closest('button');
      if (!btn || list.contains(btn)) return;
      var act = btn.dataset.act;
      if (act === 'add') self.insert(form.querySelector('[data-f=add-kind]').value);
      else if (act === 'add-text') self.insert('tekst');
      else if (act === 'undo' && self.history.length) {
        self.blocks = self.history.pop();
        self.render();
        self.sync();
        self.announce('Cofnięto ostatnią zmianę.');
      } else if (act === 'reset') {
        e.preventDefault();   // bez JS ten przycisk zapisuje domyślny; tu tylko wczytuje
        self.remember();
        self.blocks = parseLayout(form.dataset['default']);
        self.render();
        self.sync();
        self.announce('Wczytano układ domyślny. Zapisz, żeby go użyć.');
      } else if (act === 'discard') {
        self.dirtyFromServer = false;
        self.previewChars = {};
        self.initial = self.snapshot();   // bez pytania przy podmianie
        htmx.ajax('GET', '/bulletin', { target: '#bulletin-form', select: '#bulletin-form', swap: 'outerHTML' });
      }
    });
    form.addEventListener('input', function (e) {
      if (list.contains(e.target)) return;
      if (e.target.name === 'mode' || e.target.name === 'interval_min') {
        self.modeUi();
        self.blocks.forEach(function (b, i) {
          var li = list.querySelector('[data-uid="' + b.uid + '"]');
          if (li) self.refreshCard(li, b, i);
        });
      }
      self.sync();
    });
  };

  // Przeciąganie: uchwyt włącza `draggable` na karcie (tekst w polach dalej
  // da się zaznaczać myszą), znacznik pokazuje, gdzie klocek wyląduje.
  Editor.prototype.bindDrag = function () {
    var self = this, list = this.list, dragged = null, target = null, after = false;
    function clear() { all(list, '.drop-before, .drop-after').forEach(function (el) { el.classList.remove('drop-before', 'drop-after'); }); }
    var armed = null;   // karta „uzbrojona” wciśnięciem uchwytu
    list.addEventListener('pointerdown', function (e) {
      var grip = e.target.closest('[data-act=grip]');
      if (grip) { armed = grip.closest('.blk-edit'); armed.draggable = true; }
    });
    // Wciśnięcie bez przeciągania: karta znów zwykła (zaznaczanie tekstu w polach).
    function disarm() { if (armed && armed !== dragged) armed.draggable = false; armed = null; }
    document.addEventListener('pointerup', disarm);
    document.addEventListener('pointercancel', disarm);
    list.addEventListener('dragstart', function (e) {
      var li = e.target.closest && e.target.closest('.blk-edit');
      if (!li || !li.draggable) { e.preventDefault(); return; }
      dragged = li;
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', li.dataset.uid);
      requestAnimationFrame(function () { li.classList.add('is-dragging'); });
      list.classList.add('is-sorting');
    });
    list.addEventListener('dragover', function (e) {
      if (!dragged) return;
      var li = e.target.closest('.blk-edit');
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
      if (!li || li === dragged) return;
      var r = li.getBoundingClientRect();
      var a = e.clientY > r.top + r.height / 2;
      if (li !== target || a !== after) {
        clear();
        target = li;
        after = a;
        li.classList.add(a ? 'drop-after' : 'drop-before');
      }
    });
    list.addEventListener('drop', function (e) {
      if (!dragged) return;
      e.preventDefault();
      var uids = self.blocks.map(function (b) { return String(b.uid); });
      var from = uids.indexOf(dragged.dataset.uid), to = target ? uids.indexOf(target.dataset.uid) : from;
      if (target) {
        if (after) to += 1;
        if (to > from) to -= 1;
      }
      end();
      self.move(from, to);
    });
    function end() {
      clear();
      if (dragged) { dragged.classList.remove('is-dragging'); dragged.draggable = false; }
      list.classList.remove('is-sorting');
      dragged = null;
      target = null;
    }
    list.addEventListener('dragend', end);
  };

  // ---- oś czasu ------------------------------------------------------------

  Editor.prototype.chars = function (b) {
    var info = this.info(b);
    if (info.text) return { n: renderTemplate(b.text, this.callsign).length, real: true };
    if (this.previewChars[b.uid] != null) return { n: this.previewChars[b.uid], real: true, fromPreview: true };
    return { n: info.typical || 0, real: false };
  };

  // Odcinki jak w kompozytorze: włączone klocki, pauza między sąsiednimi
  // (po poprzednim + przed następnym), za długie — klocki od końca wypadają
  // (oprócz chronionych).
  Editor.prototype.segments = function () {
    var self = this, segs = [], prev = null;
    this.blocks.forEach(function (b, i) {
      if (!b.enabled) return;
      var c = self.chars(b);
      if (prev) {
        var p = Math.min(MAX_PAUSE_MS, (prev.after || 0) + (b.before || 0));
        if (p > 0) segs.push({ pause: true, s: p / 1000 });
      }
      segs.push({ b: b, i: i, s: (c.n + 1) / 10, c: c, info: self.info(b) });
      prev = b;
    });
    var total = segs.reduce(function (t, x) { return t + x.s; }, 0);
    if (this.limitS && total > this.limitS) {
      for (var k = segs.length - 1; k >= 0 && total > this.limitS; k--) {
        var x = segs[k];
        if (x.pause || x.info.protected) continue;
        x.dropped = true;
        total -= x.s;
        if (segs[k - 1] && segs[k - 1].pause) { segs[k - 1].dropped = true; total -= segs[k - 1].s; }
      }
    }
    return { segs: segs, total: total };
  };

  Editor.prototype.timeline = function () {
    var self = this, tl = this.tl;
    if (!tl) return;
    var r = this.segments(), all_s = r.segs.reduce(function (t, x) { return t + x.s; }, 0);
    var scale = Math.max(all_s, this.limitS || 0) || 1;
    tl.textContent = '';
    var bar = document.createElement('div');
    bar.className = 'tl-bar';
    r.segs.forEach(function (x) {
      var el = document.createElement(x.pause ? 'span' : 'button');
      el.style.flexGrow = Math.max(x.s, 0.2);
      if (x.pause) {
        el.className = 'tl-pause';
        el.title = 'pauza ' + secs(x.s * 1000) + ' s';
      } else {
        el.type = 'button';
        el.className = 'tl-seg k-' + x.b.kind + (x.b.when !== 'zawsze' ? ' is-sometimes' : '');
        el.title = x.info.label + ': ≈ ' + Math.round(x.s) + ' s';
        el.setAttribute('aria-label', 'Pokaż klocek ' + (x.i + 1) + ': ' + x.info.label);
        el.dataset.uid = x.b.uid;
        if (!self.editable) el.tabIndex = -1;
      }
      if (x.dropped) el.classList.add('is-dropped');
      bar.appendChild(el);
    });
    if (this.limitS && all_s > this.limitS) {
      var mark = document.createElement('span');
      mark.className = 'tl-limit';
      mark.style.left = (this.limitS / scale * 100) + '%';
      mark.title = 'limit ' + this.limitS + ' s';
      bar.appendChild(mark);
    } else {
      var rest = document.createElement('span');
      rest.className = 'tl-rest';
      rest.style.flexGrow = Math.max(scale - all_s, 0);
      bar.appendChild(rest);
    }
    tl.appendChild(bar);

    var scaleRow = document.createElement('div');
    scaleRow.className = 'tl-scale';
    [ '0 s', this.limitS ? 'limit ' + this.limitS + ' s' : Math.round(scale) + ' s' ].forEach(function (text) {
      var span = document.createElement('span');
      span.textContent = text;
      scaleRow.appendChild(span);
    });
    tl.appendChild(scaleRow);

    var legend = document.createElement('ol');
    legend.className = 'tl-list';
    r.segs.forEach(function (x) {
      if (x.pause) return;
      var li = document.createElement('li'), dot = document.createElement('span'), name = document.createElement('span'), t = document.createElement('span');
      li.className = x.dropped ? 'is-dropped' : '';
      dot.className = 'tl-dot k-' + x.b.kind;
      name.className = 'tl-name';
      name.textContent = x.info.label;
      var notes = [];
      if (x.b.when !== 'zawsze') notes.push(x.b.when === 'co_n_cykli' ? 'co ' + x.b.n + '. cykl' : x.b.od + '–' + x.b['do']);
      if (x.dropped) notes.push('wypadnie');
      if (notes.length) { var small = document.createElement('small'); small.textContent = notes.join(' · '); name.appendChild(small); }
      t.className = 'tl-time';
      t.textContent = (x.c.real ? '' : '~') + Math.max(1, Math.round(x.s)) + ' s';
      t.title = x.c.fromPreview ? 'z ostatniego podglądu' : x.c.real ? 'z tekstu' : 'typowa długość';
      li.appendChild(dot); li.appendChild(name); li.appendChild(t);
      legend.appendChild(li);
    });
    tl.appendChild(legend);

    var total = document.querySelector('[data-out=total]');
    if (total) {
      var over = this.limitS && all_s > this.limitS;
      total.classList.toggle('is-over', !!over);
      total.textContent = '';
      var strong = document.createElement('strong');
      strong.textContent = '≈ ' + Math.round(r.total) + ' s';
      total.appendChild(strong);
      total.appendChild(document.createTextNode(this.limitS ? ' z ' + this.limitS + ' s na treść' : ''));
      if (over) total.appendChild(document.createTextNode(' — za długi o ≈ ' + Math.round(all_s - this.limitS) + ' s, przekreślone klocki wypadną.'));
      else if (!r.segs.length) total.appendChild(document.createTextNode(' — żaden klocek nie jest nadawany.'));
    }
    tl.setAttribute('aria-label', 'Oś czasu: ' + r.segs.filter(function (x) { return !x.pause; }).length + ' klocków, około ' + Math.round(r.total) + ' sekund');
  };

  // Długości klocków z ostatniego podglądu (karta #bulletin-preview).
  Editor.prototype.readPreview = function () {
    var box = document.querySelector('#bulletin-preview .preview-result');
    this.previewChars = {};
    if (!box) return;
    var chars;
    try { chars = JSON.parse(box.dataset.chars || '[]'); } catch (e) { return; }
    var uids = this.previewUids;
    if (!uids) {
      // Karta podglądu sprzed tego edytora (wczytanie strony, zapis, „Odrzuć”):
      // pasuje po kolei tylko do tego samego układu.
      var same = previewFor ? previewFor === this.json() : !this.isDirtyLayout();
      if (!same) return;
      uids = this.blocks.map(function (b) { return b.uid; });
    }
    var self = this;
    // null: klocek w tym cyklu nie wszedł (np. „co 3. cykl”) — zostaje typowa długość.
    chars.forEach(function (n, i) { if (uids[i] != null && n != null) self.previewChars[uids[i]] = n; });
  };

  // ---- start i htmx ----------------------------------------------------------

  function start() {
    var form = document.getElementById('bulletin-form');
    if (!form || form.dataset.ready) return;
    form.dataset.ready = '1';
    try {
      ed = new Editor(form);
    } catch (err) {   // nieczytelny JSON (wpisany bez JS): zostaje formularz bez JS
      ed = null;
      if (window.console) console.warn('edytor biuletynu:', err);
    }
  }

  document.addEventListener('DOMContentLoaded', start);
  document.addEventListener('htmx:afterSettle', function () {
    start();
    if (ed && document.body.contains(ed.form)) { ed.readPreview(); ed.timeline(); }
  });
  // Podgląd składa układ z edytora (także niezapisany).
  document.addEventListener('htmx:configRequest', function (e) {
    if (!ed || !ed.editable || e.detail.elt.id !== 'bulletin-preview-form') return;
    e.detail.parameters.layout = ed.json();
    previewFor = e.detail.parameters.layout;
    ed.previewUids = ed.blocks.map(function (b) { return b.uid; });
  });
  // Odwołane żądanie nie dostaje htmx:afterRequest — przyciski zablokowane
  // przez panel.js (data-busy) odblokowujemy sami.
  function unbusy(form) {
    all(form, 'button').forEach(function (b) {
      if (b.dataset.busy) { b.disabled = false; delete b.dataset.busy; }
      b.classList.remove('is-busy');
    });
  }
  // Zapis z błędem w klocku: nie wysyłamy, pokazujemy błąd przy klocku.
  document.addEventListener('htmx:beforeRequest', function (e) {
    if (!ed || !ed.editable || e.detail.elt !== ed.form) return;
    var bad = null;
    ed.blocks.forEach(function (b, i) {
      b.touched = true;
      var li = ed.list.querySelector('[data-uid="' + b.uid + '"]');
      if (li) ed.refreshCard(li, b, i);
      if (!bad && li && li.classList.contains('is-invalid')) bad = li;
    });
    if (!ed.blocks.length) {
      e.preventDefault();
      unbusy(ed.form);
      document.dispatchEvent(new CustomEvent('toast', { detail: { kind: 'error', text: 'Dodaj co najmniej jeden klocek.' } }));
      return;
    }
    if (bad) {
      e.preventDefault();
      unbusy(ed.form);
      bad.scrollIntoView({ block: 'center', behavior: reduced() ? 'auto' : 'smooth' });
      var f = bad.querySelector('[aria-invalid="true"]');
      if (f) f.focus({ preventScroll: true });
      document.dispatchEvent(new CustomEvent('toast', { detail: { kind: 'error', text: 'Popraw zaznaczony klocek: ' + bad.querySelector('[data-out=error]').textContent } }));
    }
  });
  // Niezapisane zmiany: pytanie przed wyjściem ze strony.
  function leaving() { return ed && ed.editable && document.body.contains(ed.form) && ed.isDirty(); }
  window.addEventListener('beforeunload', function (e) {
    if (leaving()) { e.preventDefault(); e.returnValue = ''; }
  });
  document.addEventListener('click', function (e) {
    var a = e.target.closest && e.target.closest('a[href]');
    if (!a || !leaving() || (ed.form.contains(a))) return;
    if (!window.confirm('Układ biuletynu ma niezapisane zmiany. Wyjść bez zapisu?')) { e.preventDefault(); e.stopImmediatePropagation(); }
    else ed.dirtyFromServer = false, ed.initial = ed.snapshot();
  }, true);
  // Oś czasu: kliknięcie odcinka przewija do klocka.
  document.addEventListener('click', function (e) {
    var seg = e.target.closest && e.target.closest('.tl-seg');
    if (!seg || !ed) return;
    var li = ed.list.querySelector('[data-uid="' + seg.dataset.uid + '"]') || ed.list.children[+seg.getAttribute('aria-label').replace(/\D+(\d+).*/, '$1') - 1];
    if (!li) return;
    li.scrollIntoView({ block: 'center', behavior: reduced() ? 'auto' : 'smooth' });
    li.classList.add('is-flash');
    setTimeout(function () { li.classList.remove('is-flash'); }, 900);
    var f = li.querySelector('[data-act=grip]');
    if (f) f.focus({ preventScroll: true });
  });
  // Ctrl/Cmd+Z poza polem tekstowym: cofnij.
  document.addEventListener('keydown', function (e) {
    if (!ed || !ed.editable || !(e.ctrlKey || e.metaKey) || e.key !== 'z' || e.shiftKey) return;
    if (e.target.matches('input, textarea, select')) return;
    var undo = ed.form.querySelector('[data-act=undo]');
    if (undo && !undo.disabled) { e.preventDefault(); undo.click(); }
  });
})();
