// Mapa położenia stacji na stronie ustawień (etap 9, R18).
// Jedyna zależność to Leaflet (`window.L`, zwendorowany). Bez Leafleta albo
// bez podkładu formularz współrzędnych i lokatora działa sam. Lokatora tu nie
// liczymy: kliknięcie wpisuje współrzędne i czyści lokator, a wylicza go serwer.
(function () {
  'use strict';

  function start() {
    var el = document.getElementById('station-map');
    if (!el || !window.L) {
      return;
    }
    var L = window.L;
    var latField = document.getElementById('lat');
    var lonField = document.getElementById('lon');
    var locatorField = document.getElementById('locator');
    var note = document.getElementById('map-note');

    var lat = parseFloat(el.dataset.lat);
    var lon = parseFloat(el.dataset.lon);
    var known = isFinite(lat) && isFinite(lon);
    var map = L.map(el).setView(known ? [lat, lon] : [52.0, 19.4], known ? 12 : 6);

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
      note.textContent = 'Mapa niedostępna (brak internetu?) — wpisz współrzędne albo lokator.';
      note.hidden = false;
    });
    tiles.addTo(map);

    var marker = null;

    function fill(latlng) {
      // Po przewinięciu mapy za antypołudnik długość wychodzi poza ±180.
      latlng = latlng.wrap();
      if (latField) {
        latField.value = latlng.lat.toFixed(5);
      }
      if (lonField) {
        lonField.value = latlng.lng.toFixed(5);
      }
      if (locatorField) {
        locatorField.value = '';
      }
    }

    function place(latlng) {
      if (marker) {
        marker.setLatLng(latlng);
        return;
      }
      marker = L.marker(latlng, { draggable: true }).addTo(map);
      marker.on('dragend', function () {
        fill(marker.getLatLng());
      });
    }

    if (known) {
      place([lat, lon]);
    }
    // Wpisany ręcznie lokator ma pierwszeństwo: bez tego zapisane współrzędne
    // wygrałyby z nowym lokatorem (R17) i zmiana przepadłaby po cichu.
    if (locatorField) {
      locatorField.addEventListener('input', function () {
        if (latField) {
          latField.value = '';
        }
        if (lonField) {
          lonField.value = '';
        }
      });
    }
    map.on('click', function (event) {
      place(event.latlng);
      fill(event.latlng);
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
