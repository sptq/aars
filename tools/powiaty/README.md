# Granice powiatów do mapy w panelu

`assets/powiaty.geojson` powstaje jednorazowo, ręcznie (2026-11-22, 380 powiatów):

```sh
curl -o powiaty.gml "https://mapy.geoportal.gov.pl/wss/service/PZGIK/PRG/WFS/AdministrativeBoundaries?SERVICE=WFS&VERSION=2.0.0&REQUEST=GetFeature&TYPENAMES=ms:A02_Granice_powiatow&SRSNAME=EPSG:4326&OUTPUTFORMAT=text/xml;%20subtype=gml/3.1.1"
python3 -I tools/powiaty/gml_to_geojson.py powiaty.gml assets/powiaty.geojson
```

Źródło: Państwowy Rejestr Granic (PRG), GUGiK. Uproszczenie Douglasa-Peuckera
0,004° i zaokrąglenie do 4 miejsc (stałe w skrypcie). Właściwości: `teryt`
(4 cyfry, `JPT_KOD_JE`) i `nazwa`.
