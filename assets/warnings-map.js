// Mapa ostrzeżeń meteo IMGW na stronie /map (etap 11c).
// Jedyna zależność to Leaflet (`window.L`, zwendorowany). Granice powiatów
// pobieramy raz, dane ostrzeżeń z /map/data przy starcie i co 5 minut.
// Kolorujemy tylko przy stanie "ok": przy każdym innym stanie albo błędzie
// pobrania mapa nie może wyglądać jak „brak ostrzeżeń”, więc baner i żadnych
// kolorów.
(function () {
  'use strict';

  var REFRESH_MS = 300 * 1000;
  var COLORS = { 1: '#ffd400', 2: '#ff8c00', 3: '#d7191c' };
  var BANNERS = {
    disabled: 'Źródło ostrzeżeń meteo wyłączone — mapa nie pokazuje bieżących ostrzeżeń.',
    stale: 'Źródło ostrzeżeń meteo nieaktualne — mapa nie pokazuje bieżących ostrzeżeń.',
    never_no_area: 'Ustaw obszar źródła ostrzeżeń meteo — bez niego źródło nic nie pobiera, a mapa nie pokazuje ostrzeżeń.',
    never: 'Źródło ostrzeżeń meteo jeszcze nic nie pobrało od startu stacji — mapa nie pokazuje bieżących ostrzeżeń.',
    error: 'Nie udało się pobrać danych mapy.'
  };

  function pad(n) {
    return (n < 10 ? '0' : '') + n;
  }

  // DD.MM HH:MM w czasie lokalnym przeglądarki; zły czas → null.
  function shortTime(iso) {
    var d = new Date(iso);
    if (!iso || isNaN(d.getTime())) {
      return null;
    }
    return pad(d.getDate()) + '.' + pad(d.getMonth() + 1) + ' ' + pad(d.getHours()) + ':' + pad(d.getMinutes());
  }

  function start() {
    var el = document.getElementById('warnings-map');
    if (!el || !window.L) {
      return;
    }
    var L = window.L;
    var banner = document.getElementById('map-banner');
    var fetched = document.getElementById('map-fetched');
    var note = document.getElementById('map-note');

    var map = L.map(el).setView([52.0, 19.4], 6);
    var tiles = L.tileLayer(el.dataset.tiles, {
      maxZoom: 19,
      attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>'
    });
    var warned = false;
    tiles.on('tileerror', function () {
      if (warned || !note) {
        return;
      }
      warned = true;
      note.textContent = 'Podkład mapy niedostępny (brak internetu?) — granice powiatów i ostrzeżenia nadal są pokazane.';
      note.hidden = false;
    });
    tiles.addTo(map);

    var counties = null; // warstwa GeoJSON
    var bordersFailed = false;
    var data = null; // ostatnia odpowiedź /map/data albo null po błędzie
    var byTeryt = {}; // teryt → lista ostrzeżeń (tylko przy "ok")
    var marker = null;
    var placed = false; // widok ustawiony raz, potem użytkownik przewija sam

    function showBanner(text) {
      if (!banner) {
        return;
      }
      banner.textContent = text || '';
      banner.hidden = !text;
    }

    function inArea(teryt) {
      var area = (data && data.area) || [];
      for (var i = 0; i < area.length; i++) {
        var code = String(area[i]);
        if (code.length === 4 ? teryt === code : teryt.indexOf(code) === 0) {
          return true;
        }
      }
      return false;
    }

    function styleFor(feature) {
      var teryt = String((feature.properties && feature.properties.teryt) || '');
      var top = 0;
      (byTeryt[teryt] || []).forEach(function (w) {
        if (w.level > top) {
          top = w.level;
        }
      });
      var style = { color: '#888', weight: 0.5, fill: false, fillOpacity: 0 };
      if (COLORS[top]) {
        style.fill = true;
        style.fillColor = COLORS[top];
        style.fillOpacity = 0.55;
      }
      if (inArea(teryt)) {
        style.color = '#1f4fd1';
        style.weight = 2.5;
      }
      return style;
    }

    // Treść dymku budowana z węzłów tekstowych: nazwy zdarzeń przychodzą z sieci.
    function popupFor(feature) {
      var props = feature.properties || {};
      var box = document.createElement('div');
      var title = document.createElement('strong');
      title.textContent = props.nazwa || props.teryt || '';
      box.appendChild(title);
      var list = byTeryt[String(props.teryt || '')] || [];
      if (!list.length) {
        var none = document.createElement('div');
        none.textContent = data && data.state === 'ok' ? 'Brak ostrzeżeń.' : 'Mapa nie pokazuje bieżących ostrzeżeń.';
        box.appendChild(none);
      }
      list.forEach(function (w) {
        var line = document.createElement('div');
        line.textContent = w.event + ' (' + w.level + '. stopień, do ' + (shortTime(w.until) || '—') + ')';
        box.appendChild(line);
      });
      return box;
    }

    function placeView() {
      if (placed || !data) {
        return;
      }
      if (data.station) {
        map.setView([data.station.lat, data.station.lon], 8);
        placed = true;
        return;
      }
      if (!counties) {
        return;
      }
      var bounds = null;
      counties.eachLayer(function (layer) {
        if (inArea(String((layer.feature.properties || {}).teryt || ''))) {
          bounds = bounds ? bounds.extend(layer.getBounds()) : L.latLngBounds(layer.getBounds().getSouthWest(), layer.getBounds().getNorthEast());
        }
      });
      if (bounds) {
        map.fitBounds(bounds);
      }
      placed = true;
    }

    function paint() {
      byTeryt = {};
      if (data && data.state === 'ok') {
        (data.warnings || []).forEach(function (w) {
          var key = String(w.teryt);
          (byTeryt[key] = byTeryt[key] || []).push(w);
        });
      }
      if (counties) {
        counties.eachLayer(function (layer) {
          layer.setStyle(styleFor(layer.feature));
          if (inArea(String((layer.feature.properties || {}).teryt || ''))) {
            layer.bringToFront();
          }
        });
      }
      if (data && data.station) {
        var at = [data.station.lat, data.station.lon];
        if (marker) {
          marker.setLatLng(at);
        } else {
          marker = L.marker(at, { title: 'Stacja' }).addTo(map);
        }
      } else if (marker) {
        map.removeLayer(marker);
        marker = null;
      }
      placeView();
    }

    function bannerFor(d) {
      if (d.state === 'ok') {
        return '';
      }
      if (d.state === 'never') {
        return d.area && d.area.length ? BANNERS.never : BANNERS.never_no_area;
      }
      // "disabled", "stale" i każdy nieznany stan: baner, nigdy cisza.
      return BANNERS[d.state] || BANNERS.stale;
    }

    function failed() {
      data = null;
      showBanner(BANNERS.error);
      if (fetched) {
        fetched.textContent = '—';
      }
      paint();
    }

    function load() {
      fetch(el.dataset.src, { credentials: 'same-origin', headers: { Accept: 'application/json' } })
        .then(function (res) {
          if (!res.ok) {
            throw new Error('HTTP ' + res.status);
          }
          return res.json();
        })
        .then(function (d) {
          if (!d || typeof d.state !== 'string') {
            throw new Error('zły kształt danych');
          }
          data = d;
          showBanner(bordersFailed ? BANNERS.error : bannerFor(d));
          if (fetched) {
            var when = shortTime(d.fetched_at);
            fetched.textContent = when ? 'Pobrano: ' + when : '—';
          }
          paint();
        })
        .catch(failed);
    }

    fetch(el.dataset.geojson, { credentials: 'same-origin' })
      .then(function (res) {
        if (!res.ok) {
          throw new Error('HTTP ' + res.status);
        }
        return res.json();
      })
      .then(function (geo) {
        counties = L.geoJSON(geo, { style: styleFor }).addTo(map);
        counties.eachLayer(function (layer) {
          layer.bindPopup(function () {
            return popupFor(layer.feature);
          });
        });
        paint();
      })
      .catch(function () {
        // Bez granic nie ma czego kolorować — mapa nie może udawać braku ostrzeżeń.
        bordersFailed = true;
        showBanner(BANNERS.error);
      });

    load();
    window.setInterval(load, REFRESH_MS);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
